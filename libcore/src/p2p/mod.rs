//! Direct peer-to-peer transport: connect through the relay's TURN bridge
//! immediately — so a link is usable in about one round trip — and punch a
//! NAT hole in the background. When the punch validates a direct path, the
//! socket swaps that connection's egress to raw UDP ([`TurnRoutes`]): same
//! QUIC connection, same synthetic peer address, bulk traffic now
//! device-to-device.
//!
//! Candidates ride the existing MLS channel ([`signal`]). Bottom-up: the
//! poke wire ([`disco`]) and the socket that carries it ([`socket`]); the
//! punch state machine ([`punch`]); local candidates ([`candidate`]); and
//! here, the session manager that ties them together.
//!
//! One [`connect`] call per peer: the lower IPK dials, the higher accepts,
//! so exactly one connection forms. Peers with no relay on record punch
//! first and connect direct — the only path they have.

#![allow(dead_code)]

mod candidate;
pub(crate) mod consent;
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
use crate::data::identity::Identity;
use crate::utils::addr_short;
use crate::utils::addrs_short;
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
/// Wait this long for the peer's candidate offer — in the background on
/// the relay-first path, in the foreground when the punch is the only
/// path. Generous: the offer crosses the store-and-forward relay, and the
/// auto-accept side needs a round trip to answer.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(30);
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The acceptor waits this long for the inbound connection. The dialer
/// connects over the bridge as soon as its offer is out, so the inbound
/// lands about one relay round trip after our `TurnAlloc`.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
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
    /// Live links keyed by peer IPK, so signaling and transfer reuse one
    /// connection instead of re-dialing. See [`link`].
    links: Mutex<HashMap<[u8; 32], PeerLink>>,
}

static P2P: OnceCell<P2pEndpoint> = OnceCell::new();

/// Build the P2P endpoint once and spawn the loop that routes each inbound
/// poke to the session owning its channel. Must be called from the tokio
/// runtime.
fn endpoint() -> Result<&'static P2pEndpoint> {
    P2P.get_or_try_init(|| {
        let built = socket::build_endpoint()?;
        let local = built.endpoint.local_addr()?;
        let port = local.port();
        log::info!("P2P: endpoint bound to {}", addr_short(local));
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
            links: Mutex::new(HashMap::new()),
        })
    })
}

/// The disco routing channel for a peer pair — a *public* tag (only the
/// disco key is secret; see [`disco`]), derived deterministically from the
/// sorted IPKs so both ends agree on it before any offer, with no MLS
/// lookup. The secret key itself rides the offer (see [`run_session`]), so
/// the punch works even if the two sides' groups/epochs differ.
fn channel_for(a: &[u8; 32], b: &[u8; 32]) -> [u8; 8] {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"promtuz/p2p/chan");
    hasher.update(lo);
    hasher.update(hi);
    let mut chan = [0u8; 8];
    chan.copy_from_slice(&hasher.finalize().as_bytes()[..8]);
    chan
}

/// `N` fresh random bytes — a session secret (disco key / bridge token)
/// generated per connect and exchanged in the offer.
fn rand_bytes<const N: usize>() -> [u8; N] {
    use ed25519_dalek::ed25519::signature::rand_core::OsRng;
    use ed25519_dalek::ed25519::signature::rand_core::RngCore;
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
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
            log::info!("P2P: reflexive address {}", addr_short(seen));
            let _ = report.send(Some(seen));
            return;
        }
    }
}

/// Aborts a background task when dropped — bounds the TURN keepalive to
/// its route's lifetime across every return path.
struct AbortGuard(tokio::task::JoinHandle<()>);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Deregisters a TURN route when dropped, across every return path (the
/// token is decided inside the session, not the caller).
struct TurnGuard(&'static P2pEndpoint, [u8; 16]);
impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.turn.lock().unregister(&self.1);
    }
}

type RouteGuards = (AbortGuard, TurnGuard);

/// Register the TURN bridge and keep its NAT mapping to the relay warm.
/// Returns the synthetic address quinn dials/accepts for it, plus the
/// guards that tear the route down.
fn open_turn_route(
    ep: &'static P2pEndpoint, token: [u8; 16], relay: SocketAddr,
) -> (SocketAddr, RouteGuards) {
    let synth = ep.turn.lock().register(token, relay);
    // Re-send the TurnAlloc every few seconds to keep the NAT mapping to
    // the relay warm. A symmetric NAT (the case that forces TURN) drops an
    // idle per-destination mapping — without this the return path is
    // stranded at a stale source the relay never registered.
    let pokes = ep.pokes.clone();
    let alloc = RelayMsg::TurnAlloc { token }.encode();
    let keepalive = RUNTIME.spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(4));
        loop {
            tick.tick().await; // fires immediately, then every 4s
            if pokes.send(relay, &alloc).await.is_err() {
                break;
            }
        }
    });
    (synth, (AbortGuard(keepalive), TurnGuard(ep, token)))
}

/// Quinn egresses to the synth for the connection's whole life, so the
/// route (and its keepalive) must outlive `run_session` — park the guards
/// on a task bound to the connection.
fn hold_route_while_open(conn: Connection, guards: RouteGuards) {
    RUNTIME.spawn(async move {
        conn.closed().await;
        drop(guards);
    });
}

/// Unroutes a session's pokes and offer listener when dropped — by
/// `run_session` on its foreground paths, or by the background punch task,
/// which can outlive the session. A newer session for the same peer reuses
/// the same chan, so the poke route is only removed if it is still ours.
struct SessionCleanup {
    ep:   &'static P2pEndpoint,
    chan: [u8; 8],
    peer: [u8; 32],
    tx:   mpsc::UnboundedSender<Poke>,
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        {
            let mut sessions = self.ep.sessions.lock();
            if sessions.get(&self.chan).is_some_and(|t| t.same_channel(&self.tx)) {
                sessions.remove(&self.chan);
            }
        }
        signal::stop(self.peer);
    }
}

/// Background upgrade: punch, and on a validated address flip the bridge's
/// egress to direct. `set_direct` returning false means the route (and its
/// connection) died first — nothing to upgrade.
async fn punch_upgrade(
    ep: &'static P2pEndpoint, mut poke_rx: mpsc::UnboundedReceiver<Poke>, key: DiscoKey,
    cands: Vec<SocketAddr>, token: [u8; 16], peer: [u8; 32],
) {
    match punch::punch(&ep.pokes, &mut poke_rx, key, cands, PUNCH_TIMEOUT).await {
        Some(addr) if ep.turn.lock().set_direct(&token, addr) => {
            log::info!("P2P[{}]: upgraded to direct {}", hex::encode(&peer[..4]), addr_short(addr));
        },
        Some(_) => {},
        None => log::debug!("P2P[{}]: no direct path — staying relayed", hex::encode(&peer[..4])),
    }
}

/// A live direct connection to a peer.
#[derive(Clone)]
pub struct PeerLink {
    pub(crate) conn: Connection,
    dialer: bool,
    pub ipk: [u8; 32],
}

impl PeerLink {
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    pub async fn open_stream(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        Ok(self.conn.open_bi().await?)
    }

    pub async fn accept_stream(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        Ok(self.conn.accept_bi().await?)
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

/// Wrap a raw connection as a [`PeerLink`] so transfer tests can drive
/// serve/pull over a direct loopback pair without the punch choreography.
#[cfg(test)]
pub(crate) fn test_link(conn: Connection, ipk: [u8; 32]) -> PeerLink {
    PeerLink { conn, dialer: false, ipk }
}

/// The bail `connect` emits when a dial to this peer is already in flight;
/// `link` matches on it to wait for the winner instead of failing.
const ALREADY_CONNECTING: &str = "already connecting to that peer";

/// Open a direct connection to `peer`: trade candidates over MLS, punch a
/// hole, then dial (lower IPK) or accept (higher IPK) over the validated
/// address. Both peers call this; the IPK order decides who dials, so
/// exactly one connection forms.
pub async fn connect(peer: [u8; 32]) -> Result<PeerLink> {
    if !CONNECTING.lock().insert(peer) {
        bail!("{ALREADY_CONNECTING}");
    }
    if matches!(consent::may_connect(&peer), consent::Decision::No) {
        CONNECTING.lock().remove(&peer);
        bail!("consent: not permitted to connect to that peer");
    }
    let result = connect_inner(peer).await;
    CONNECTING.lock().remove(&peer);
    result
}

async fn connect_inner(peer: [u8; 32]) -> Result<PeerLink> {
    let ep = endpoint()?;
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
    let chan = channel_for(&our_ipk, &peer);

    // Route this session's pokes and listen for the peer's offer before we
    // announce ourselves, so nothing races ahead of the registration. The
    // channel is public and IPK-derived, so we know it here (the secret key
    // comes with the offer). Teardown rides the cleanup guard: run_session
    // either drops it on return or hands it to its background punch task,
    // which needs the routes alive after the connect completes.
    let (poke_tx, poke_rx) = mpsc::unbounded_channel();
    ep.sessions.lock().insert(chan, poke_tx.clone());
    let offers = signal::listen(peer);
    let cleanup = SessionCleanup { ep, chan, peer, tx: poke_tx };

    let result = run_session(ep, our_ipk, chan, poke_rx, offers, cleanup, peer).await;

    // Single terminal outcome per attempt: warn with the reason on failure,
    // info with the winning route on success.
    let (link, route) = match result {
        Ok(v) => v,
        Err(e) => {
            log::warn!("P2P[{}]: connection failed — {e}", hex::encode(&peer[..4]));
            return Err(e);
        },
    };
    // Prove the link both ways (dialer pings, acceptor answers) so the connect
    // is self-verifying before we hand it out.
    if let Err(e) = link.verify_roundtrip().await {
        log::warn!("P2P[{}]: link verify failed — {e}", hex::encode(&peer[..4]));
        link.conn.close(0u32.into(), b"verify failed");
        return Err(e);
    }
    log::info!(
        "P2P[{}]: connected via {route} — {}",
        hex::encode(&peer[..4]),
        addr_short(link.remote_address())
    );
    ep.links.lock().insert(peer, link.clone());
    // Both ends serve pulls for whatever they retain, so a file offered either
    // direction is fetchable over this one link.
    RUNTIME.spawn(crate::transfer::serve_link(link.clone()));
    Ok(link)
}

/// Build the P2P endpoint (and its accept/routing loop) if it isn't up yet.
/// The reverse-wake path calls this after a push revives us, so we're ready to
/// accept the receiver's retry-dial.
pub fn ensure_endpoint() -> Result<()> {
    endpoint().map(|_| ())
}

/// Return a live link to `peer`, reusing an open one or forming a new connection.
pub async fn link(peer: [u8; 32]) -> Result<PeerLink> {
    let ep = endpoint()?;
    // Reuse-or-prune under one lock: return a live, still-consented link; sever
    // a revoked one; drop a dead one. A separate get-then-remove could evict a
    // link a concurrent dialer just inserted.
    {
        let mut links = ep.links.lock();
        if let Some(l) = links.get(&peer).cloned() {
            if l.conn.close_reason().is_none() {
                // Re-gate on reuse: a live QUIC link must not outlive consent.
                // Unpaired/forgotten since it opened → sever, don't hand back.
                if matches!(consent::may_connect(&peer), consent::Decision::Direct) {
                    return Ok(l);
                }
                links.remove(&peer);
                drop(links);
                l.conn.close(0u32.into(), b"consent revoked");
                bail!("consent: not permitted to connect to that peer");
            }
            // ponytail: prune-on-dead is enough for v1; a timed idle sweep is a
            // later optimization.
            links.remove(&peer);
        }
    }
    // Cold IPK: dial. connect() gates on consent and dedups concurrent dials
    // via CONNECTING; a caller that loses that race waits for the winner's link
    // instead of surfacing a spurious "already connecting".
    match connect(peer).await {
        Ok(l) => Ok(l),
        Err(e) if e.to_string() == ALREADY_CONNECTING => wait_for_cached_link(ep, peer).await,
        Err(e) => Err(e),
    }
}

/// Wait for the in-flight dial to `peer` (started by another caller) to publish
/// its link. ponytail: fixed-interval poll of the link cache; a shared
/// dial-future would drop the wakeup latency, worth it only if cold-dial
/// contention gets common.
async fn wait_for_cached_link(ep: &'static P2pEndpoint, peer: [u8; 32]) -> Result<PeerLink> {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
    loop {
        if let Some(l) = ep.links.lock().get(&peer).cloned()
            && l.conn.close_reason().is_none()
        {
            return Ok(l);
        }
        // Winner cleared CONNECTING. It publishes the link (connect_inner)
        // before clearing the flag (connect), so once the flag is gone one more
        // cache check is authoritative: present → use it, still absent → the
        // dial failed. Without this re-check a loser that sampled the cache just
        // before the winner's insert-then-clear would bail on a live link.
        if !CONNECTING.lock().contains(&peer) {
            if let Some(l) = ep.links.lock().get(&peer).cloned()
                && l.conn.close_reason().is_none()
            {
                return Ok(l);
            }
            bail!("in-flight dial to {} finished without a link", hex::encode(&peer[..4]));
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out waiting for in-flight dial to {}", hex::encode(&peer[..4]));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Sever any live direct link to `peer` — the P2P half of the forget/unpair
/// cascade, so a revoked contact's open QUIC connection dies with the pairing.
/// Best-effort: a no-op if the endpoint was never built or no link is open.
pub(crate) fn drop_link(peer: &[u8; 32]) {
    if let Some(ep) = P2P.get() {
        if let Some(link) = ep.links.lock().remove(peer) {
            link.conn.close(0u32.into(), b"contact forgotten");
        }
    }
}

async fn run_session(
    ep: &'static P2pEndpoint,
    our_ipk: [u8; 32],
    chan: [u8; 8],
    poke_rx: mpsc::UnboundedReceiver<Poke>,
    mut offers: mpsc::UnboundedReceiver<Offer>,
    cleanup: SessionCleanup,
    peer: [u8; 32],
) -> Result<(PeerLink, &'static str)> {
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

    // Publish our candidates (local + reflexive), home relay, and our random
    // session secrets (bridge token + disco key). The bridge, the disco key,
    // and the punch channel are all the dialer's, so the dialer needs
    // nothing back before it connects — only the punch waits on the peer's
    // candidates.
    let our_relay = home_relay_turn_addr();
    let my_token = rand_bytes::<16>();
    let my_disco_key = rand_bytes::<32>();
    let mut cands = candidate::local_candidates(ep.port);
    if let Some(r) = reflexive {
        cands.push(r);
    }
    signal::send_offer(peer, cands, our_relay, my_token, my_disco_key).await?;

    let dialer = our_ipk < peer;

    if dialer && let Some(tr) = our_relay {
        // Relay-first: dial the bridge now so the link is usable in about a
        // round trip; the punch runs behind it and upgrades the socket's
        // egress in place when a direct path validates.
        log::info!("P2P[{}]: dialer, connecting via relay bridge", hex::encode(&peer[..4]));
        let (synth, guards) = open_turn_route(ep, my_token, tr);
        let key = DiscoKey::new(&my_disco_key, chan);
        RUNTIME.spawn(async move {
            let _cleanup = cleanup;
            let Ok(Some(offer)) = timeout(SIGNAL_TIMEOUT, offers.recv()).await else {
                log::debug!("P2P[{}]: no peer offer — staying relayed", hex::encode(&peer[..4]));
                return;
            };
            log::info!(
                "P2P[{}]: peer offers [{}], punching in background",
                hex::encode(&peer[..4]),
                addrs_short(&offer.candidates)
            );
            punch_upgrade(ep, poke_rx, key, offer.candidates, my_token, peer).await;
        });
        let conn = ep.endpoint.connect(synth, PEER_SNI)?.await?;
        hold_route_while_open(conn.clone(), guards);
        return Ok((PeerLink { conn, dialer: true, ipk: peer }, "relay"));
    }

    // Both remaining roles need the peer's offer first: the no-relay dialer
    // for the punch targets, the acceptor for the dialer's secrets.
    let offer = timeout(SIGNAL_TIMEOUT, offers.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for peer candidates"))?
        .ok_or_else(|| anyhow!("candidate listener closed"))?;
    log::info!(
        "P2P[{}]: {}, peer offers [{}]",
        hex::encode(&peer[..4]),
        if dialer { "dialer (no relay)" } else { "acceptor" },
        addrs_short(&offer.candidates)
    );

    if dialer {
        // No bridge to lean on: the punch is the only path (still serves
        // un-NATed global-IPv6 peers).
        let key = DiscoKey::new(&my_disco_key, chan);
        let mut poke_rx = poke_rx;
        let addr = punch::punch(&ep.pokes, &mut poke_rx, key, offer.candidates, PUNCH_TIMEOUT)
            .await
            .ok_or_else(|| anyhow!("no relay and no direct path"))?;
        log::info!("P2P[{}]: hole punched, dialing {}", hex::encode(&peer[..4]), addr_short(addr));
        let conn = ep.endpoint.connect(addr, PEER_SNI)?.await?;
        return Ok((PeerLink { conn, dialer: true, ipk: peer }, "direct"));
    }

    // Acceptor: bridge through the dialer's relay under the dialer's token,
    // and run the punch in the background — it opens our NAT for the
    // dialer's packets, and its validated address upgrades a relayed
    // connection to direct (each side learns the peer's real address from
    // its own pong; no extra signaling).
    let key = DiscoKey::new(&offer.disco_key, chan);
    let token = offer.token;
    let guards = offer.relay.map(|tr| open_turn_route(ep, token, tr).1);
    let peer_cands = offer.candidates;
    RUNTIME.spawn(async move {
        let _cleanup = cleanup;
        punch_upgrade(ep, poke_rx, key, peer_cands, token, peer).await;
    });
    log::info!("P2P[{}]: acceptor waiting for inbound", hex::encode(&peer[..4]));
    let incoming = timeout(ACCEPT_TIMEOUT, ep.endpoint.accept())
        .await
        .map_err(|_| anyhow!("timed out waiting for inbound connection"))?
        .ok_or_else(|| anyhow!("endpoint closed"))?;
    // ponytail: MVP accepts the first inbound. Only this peer knows our
    // punched address (candidates went over E2E MLS) and the TURN token
    // (MLS-derived), and peer TLS gates on a valid IPK cert — but the
    // real filter is matching the accepted connection's IPK to `peer`;
    // add when >1 concurrent session is possible.
    let conn = incoming.accept()?.await?;
    if let Some(g) = guards {
        hold_route_while_open(conn.clone(), g);
    }
    Ok((PeerLink { conn, dialer: false, ipk: peer }, "inbound"))
}
