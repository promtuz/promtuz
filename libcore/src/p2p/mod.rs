//! Direct peer-to-peer transport: punch a NAT hole and stand up a direct
//! QUIC link between two clients, so calls and >256KB transfers skip the
//! store-and-forward relay.
//!
//! The relay stays the fallback and the signaling path — candidates ride
//! the existing MLS channel ([`signal`]) — but bulk/live traffic goes
//! straight device-to-device once a hole is open. Bottom-up: the poke
//! wire ([`disco`]) and the socket that carries it ([`socket`]); the punch
//! state machine ([`punch`]); local candidates ([`candidate`]); and here,
//! the session manager that ties them together.
//!
//! One [`connect`] call per peer: derive a shared disco key from the MLS
//! group, trade candidates, punch, then connect (lower IPK) or accept
//! (higher IPK) over the validated address.

#![allow(dead_code)]

mod candidate;
mod disco;
mod punch;
mod signal;
mod socket;

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::p2p_relay::RelayMsg;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use quinn::Connection;
use quinn::Endpoint;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::RUNTIME;
use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::mls::group::MlsGroupHandle;
use crate::mls::provider::PromtuzMlsProvider;
use disco::DiscoKey;
use signal::Offer;
use socket::Poke;
use socket::PokeSender;
use socket::StunReply;
use socket::TurnRoutes;

/// Inbound P2P candidate offer, routed from the MLS dispatch
/// (`quic/server.rs`) to the session waiting for that peer.
pub(crate) use signal::deliver as deliver_offer;

/// TLS SNI for peer connections. The peer verifier pins the IPK, not the
/// name, so any stable string does.
const PEER_SNI: &str = "peer";
/// Wait this long for the peer's candidate offer, then for the punch.
/// Generous on the signal leg: the offer crosses the store-and-forward
/// relay, and the auto-accept side needs a round trip to answer.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The acceptor waits this long for the inbound connection — long enough for
/// the dialer to exhaust its punch window and fall back to TURN.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(25);
/// How long the one-shot reflexive-address probe waits for the relay's echo.
const STUN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a session delays its offer waiting for the reflexive probe, so
/// the offer can carry the reflexive candidate. Immediate once probed.
const REFLEXIVE_WAIT: Duration = Duration::from_millis(600);

/// Peers we're mid-connect to. Guards against a second session (e.g. the
/// auto-accept below) racing a button-initiated one for the same peer.
static CONNECTING: Lazy<Mutex<HashSet<[u8; 32]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Disco channel → the session waiting on pokes for it. The receive loop
/// routes each inbound poke to the right session by its channel tag.
type Sessions = Arc<Mutex<HashMap<[u8; 8], mpsc::UnboundedSender<Poke>>>>;

/// The one P2P endpoint (built lazily on first [`connect`]), its poke
/// sender, and the routing table its receive loop feeds.
struct P2pEndpoint {
    endpoint: Endpoint,
    pokes:    PokeSender,
    port:     u16,
    sessions: Sessions,
    /// Token → synthetic-address routing for TURN-bridged sessions, shared
    /// with the socket's send/recv demux.
    turn:     Arc<Mutex<TurnRoutes>>,
    /// Our server-reflexive address from the relay's STUN echo — a watch a
    /// session briefly awaits so the first offer can carry it. Probed once at
    /// build; stays `None` if the probe never answers.
    reflexive: tokio::sync::watch::Receiver<Option<SocketAddr>>,
}

static P2P: OnceCell<P2pEndpoint> = OnceCell::new();

/// Build the P2P endpoint once and spawn the loop that routes each inbound
/// poke to the session owning its channel. Must be called from the tokio
/// runtime.
fn endpoint() -> Result<&'static P2pEndpoint> {
    P2P.get_or_try_init(|| {
        let built = socket::build_endpoint()?;
        let port = built.endpoint.local_addr()?.port();
        log::info!("P2P: endpoint bound to {:?}", built.endpoint.local_addr());
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        let mut inbox = built.inbox;
        let routes = sessions.clone();
        RUNTIME.spawn(async move {
            while let Some((src, bytes)) = inbox.recv().await {
                if let Some(chan) = disco::peek_channel(&bytes)
                    && let Some(tx) = routes.lock().get(&chan)
                {
                    let _ = tx.send((src, bytes));
                }
            }
        });

        // One-shot reflexive-address probe: ask our home relay what public
        // address this socket maps to, published on a watch a session awaits.
        let (reflexive_tx, reflexive) = tokio::sync::watch::channel(None);
        RUNTIME.spawn(reflexive_probe(built.pokes.clone(), built.stun_rx, reflexive_tx));

        Ok(P2pEndpoint {
            endpoint: built.endpoint,
            pokes: built.pokes,
            port,
            sessions,
            turn: built.turn,
            reflexive,
        })
    })
}

/// The shared disco key + TURN token for `peer`, both derived from the MLS
/// group exporter so neither needs a separate exchange: the 40-byte disco
/// secret (32-byte AEAD key + 8-byte channel tag) and a 16-byte TURN token.
fn session_secrets(peer: &[u8; 32]) -> Result<(DiscoKey, [u8; 16])> {
    let provider = PromtuzMlsProvider::shared();
    let gid = Contact::get(peer)
        .and_then(|c| c.inner.mls_group_id)
        .ok_or_else(|| anyhow!("no MLS group with peer"))?;
    let group = MlsGroupHandle::load(&provider, &gid)
        .map_err(|e| anyhow!("load group: {e}"))?
        .ok_or_else(|| anyhow!("no local group state"))?;
    let secret = group
        .export_secret(&provider, "promtuz/p2p/disco", &[], 40)
        .map_err(|e| anyhow!("export disco secret: {e}"))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&secret[..32]);
    let mut chan = [0u8; 8];
    chan.copy_from_slice(&secret[32..40]);
    let tsec = group
        .export_secret(&provider, "promtuz/p2p/turn", &[], 16)
        .map_err(|e| anyhow!("export turn token: {e}"))?;
    let mut token = [0u8; 16];
    token.copy_from_slice(&tsec[..16]);
    Ok((DiscoKey::new(&key, chan), token))
}

/// Our home relay's address — where the TURN bridge lives (assist shares
/// the relay's QUIC port). `None` if we have no relay on record.
fn home_relay_turn_addr() -> Option<SocketAddr> {
    let relay = crate::data::relay::Relay::fetch_best().ok()?;
    let ip: IpAddr = relay.host.parse().ok()?;
    Some(SocketAddr::new(ip, relay.port))
}

/// Probe our server-reflexive address once via the relay's STUN echo and
/// cache it. Peer-independent, so a single probe seeds every session's
/// offer; a stale mapping self-heals through the punch ping exchange, and
/// TURN covers whatever the reflexive candidate can't.
async fn reflexive_probe(
    pokes: PokeSender, mut stun_rx: mpsc::UnboundedReceiver<StunReply>,
    report: tokio::sync::watch::Sender<Option<SocketAddr>>,
) {
    let Some(relay) = home_relay_turn_addr() else {
        log::info!("P2P: no relay on record for the STUN reflexive probe");
        return;
    };
    let mut tx = [0u8; 8];
    {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;
        OsRng.fill_bytes(&mut tx);
    }
    if pokes.send(relay, &RelayMsg::StunReq { tx }.encode()).await.is_err() {
        return;
    }
    let deadline = tokio::time::Instant::now() + STUN_TIMEOUT;
    while let Ok(Some((rtx, seen))) = tokio::time::timeout_at(deadline, stun_rx.recv()).await {
        if rtx == tx {
            log::info!("P2P: reflexive address {seen}");
            let _ = report.send(Some(seen));
            return;
        }
    }
}

/// Aborts a background task when dropped — bounds a session's TURN
/// keepalive to the session's lifetime across every return path.
struct AbortGuard(tokio::task::JoinHandle<()>);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A live direct connection to a peer.
pub struct PeerLink {
    conn: Connection,
    dialer: bool,
}

impl PeerLink {
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// One bi-stream ping/pong to prove the link end-to-end. Dialer sends
    /// `ping` and expects `pong`; the acceptor answers. Used by the debug
    /// connect to confirm a punched link actually carries data.
    pub async fn verify_roundtrip(&self) -> Result<()> {
        if self.dialer {
            let (mut send, mut recv) = self.conn.open_bi().await?;
            send.write_all(b"ping").await?;
            send.finish()?;
            let got = recv.read_to_end(16).await?;
            if got != b"pong" {
                bail!("unexpected reply: {got:?}");
            }
        } else {
            let (mut send, mut recv) = self.conn.accept_bi().await?;
            let got = recv.read_to_end(16).await?;
            if got != b"ping" {
                bail!("unexpected request: {got:?}");
            }
            send.write_all(b"pong").await?;
            send.finish()?;
        }
        Ok(())
    }
}

/// Open a direct connection to `peer`: trade candidates over MLS, punch a
/// hole, then dial (lower IPK) or accept (higher IPK) over the validated
/// address. Both peers call this; the IPK order decides who dials, so
/// exactly one connection forms.
pub async fn connect(peer: [u8; 32]) -> Result<PeerLink> {
    if !CONNECTING.lock().insert(peer) {
        bail!("already connecting to that peer");
    }
    let result = connect_inner(peer).await;
    CONNECTING.lock().remove(&peer);
    result
}

async fn connect_inner(peer: [u8; 32]) -> Result<PeerLink> {
    let ep = endpoint()?;
    let (key, token) = session_secrets(&peer)?;
    let chan = key.channel();

    // Route this session's pokes and listen for the peer's offer before we
    // announce ourselves, so nothing races ahead of the registration.
    let (poke_tx, poke_rx) = mpsc::unbounded_channel();
    ep.sessions.lock().insert(chan, poke_tx);
    let mut offers = signal::listen(peer);

    let result = run_session(ep, key, token, poke_rx, &mut offers, peer).await;

    ep.sessions.lock().remove(&chan);
    ep.turn.lock().unregister(&token);
    signal::stop(peer);

    // Prove the link both ways as part of connecting (dialer pings, acceptor
    // answers), so one debug tap on either side is self-verifying.
    let link = result?;
    link.verify_roundtrip().await?;
    log::info!("P2P[{}]: link verified — {}", hex::encode(&peer[..4]), link.remote_address());
    Ok(link)
}

async fn run_session(
    ep: &'static P2pEndpoint,
    key: DiscoKey,
    token: [u8; 16],
    mut poke_rx: mpsc::UnboundedReceiver<Poke>,
    offers: &mut mpsc::UnboundedReceiver<Offer>,
    peer: [u8; 32],
) -> Result<PeerLink> {
    // Wait briefly for the one-shot reflexive probe so our first offer can
    // carry the server-reflexive address (it makes a cone-NAT peer punchable
    // instead of forcing the bridge). Immediate once the probe has answered.
    let mut refl_rx = ep.reflexive.clone();
    let _ = timeout(REFLEXIVE_WAIT, async {
        while refl_rx.borrow().is_none() {
            if refl_rx.changed().await.is_err() {
                break; // probe finished without a reflexive address
            }
        }
    })
    .await;
    let reflexive = *refl_rx.borrow();

    // Publish our candidates (local + reflexive) and home relay, wait for
    // theirs.
    let our_relay = home_relay_turn_addr();
    let mut cands = candidate::local_candidates(ep.port);
    if let Some(r) = reflexive {
        cands.push(r);
    }
    signal::send_offer(peer, cands, our_relay).await?;
    let offer = timeout(SIGNAL_TIMEOUT, offers.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for peer candidates"))?
        .ok_or_else(|| anyhow!("candidate listener closed"))?;
    let peer_cands = offer.candidates;

    let our_ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
    let dialer = our_ipk < peer;
    log::info!(
        "P2P[{}]: {} — {} peer candidates: {:?}",
        hex::encode(&peer[..4]),
        if dialer { "dialer" } else { "acceptor" },
        peer_cands.len(),
        peer_cands
    );

    // TURN fallback: bridge through the dialer's home relay — both ends must
    // use the same one. Register it now so it's ready if the punch fails.
    let turn_relay = if dialer { our_relay } else { offer.relay };
    let (turn_synth, _keepalive) = match turn_relay {
        Some(tr) => {
            let synth = ep.turn.lock().register(token, tr);
            // Re-send the TurnAlloc every few seconds to keep the NAT mapping
            // to the relay warm. A symmetric NAT (the case that forced TURN)
            // drops an idle per-destination mapping, and the ~10s punch window
            // is all relay-silence — without this the return path is stranded
            // at a stale source the relay never registered.
            let pokes = ep.pokes.clone();
            let alloc = RelayMsg::TurnAlloc { token }.encode();
            let keepalive = RUNTIME.spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(4));
                loop {
                    tick.tick().await; // fires immediately, then every 4s
                    if pokes.send(tr, &alloc).await.is_err() {
                        break;
                    }
                }
            });
            (Some(synth), Some(AbortGuard(keepalive)))
        },
        None => (None, None),
    };

    if dialer {
        // Dialer: punch, then connect to whichever path opened — the
        // validated direct address, else the TURN bridge's synthetic address.
        let punched = punch::punch(&ep.pokes, &mut poke_rx, key, peer_cands, PUNCH_TIMEOUT).await;
        let addr = match (punched, turn_synth) {
            (Some(a), _) => {
                log::info!("P2P[{}]: punched, dialing {}", hex::encode(&peer[..4]), a);
                a
            },
            (None, Some(s)) => {
                log::info!("P2P[{}]: punch failed, dialing via TURN", hex::encode(&peer[..4]));
                s
            },
            (None, None) => bail!("hole-punch failed and no relay for TURN"),
        };
        let conn = ep.endpoint.connect(addr, PEER_SNI)?.await?;
        Ok(PeerLink { conn, dialer: true })
    } else {
        // Acceptor: run the punch in the background purely to open our NAT
        // (its validation result doesn't matter — the hole is what counts),
        // and accept the dialer's connection — direct, or presented from the
        // synthetic address by the socket if it arrived over TURN.
        let pokes = ep.pokes.clone();
        let engine = RUNTIME.spawn(async move {
            let mut rx = poke_rx;
            let _ = punch::punch(&pokes, &mut rx, key, peer_cands, PUNCH_TIMEOUT).await;
        });
        log::info!("P2P[{}]: acceptor — waiting for inbound QUIC", hex::encode(&peer[..4]));
        let incoming = timeout(ACCEPT_TIMEOUT, ep.endpoint.accept())
            .await
            .map_err(|_| anyhow!("timed out waiting for inbound connection"))?
            .ok_or_else(|| anyhow!("endpoint closed"))?;
        log::info!(
            "P2P[{}]: inbound connection from {}",
            hex::encode(&peer[..4]),
            incoming.remote_address()
        );
        // ponytail: MVP accepts the first inbound. Only this peer knows our
        // punched address (candidates went over E2E MLS) and the TURN token
        // (MLS-derived), and peer TLS gates on a valid IPK cert — but the
        // real filter is matching the accepted connection's IPK to `peer`;
        // add when >1 concurrent session is possible.
        let conn = incoming.accept()?.await?;
        engine.abort();
        Ok(PeerLink { conn, dialer: false })
    }
}
