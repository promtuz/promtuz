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
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
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
use socket::Poke;
use socket::PokeSender;

/// Inbound P2P candidate offer, routed from the MLS dispatch
/// (`quic/server.rs`) to the session waiting for that peer.
pub(crate) use signal::deliver as deliver_offer;

/// TLS SNI for peer connections. The peer verifier pins the IPK, not the
/// name, so any stable string does.
const PEER_SNI: &str = "peer";
/// Wait this long for the peer's candidate offer, then for the punch.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Disco channel → the session waiting on pokes for it. The receive loop
/// routes each inbound poke to the right session by its channel tag.
type Sessions = Arc<Mutex<HashMap<[u8; 8], mpsc::UnboundedSender<Poke>>>>;

/// The one P2P endpoint (built lazily on first [`connect`]), its poke
/// sender, and the routing table its receive loop feeds.
struct P2pEndpoint {
    endpoint: Endpoint,
    pokes: PokeSender,
    port: u16,
    sessions: Sessions,
}

static P2P: OnceCell<P2pEndpoint> = OnceCell::new();

/// Build the P2P endpoint once and spawn the loop that routes each inbound
/// poke to the session owning its channel. Must be called from the tokio
/// runtime.
fn endpoint() -> Result<&'static P2pEndpoint> {
    P2P.get_or_try_init(|| {
        let (endpoint, pokes, mut inbox) = socket::build_endpoint()?;
        let port = endpoint.local_addr()?.port();
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

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

        Ok(P2pEndpoint { endpoint, pokes, port, sessions })
    })
}

/// The shared disco key for `peer` — the same 40 bytes both sides derive
/// from the MLS group exporter (32-byte AEAD key + 8-byte channel tag), so
/// no separate key exchange is needed.
fn disco_key(peer: &[u8; 32]) -> Result<DiscoKey> {
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
    Ok(DiscoKey::new(&key, chan))
}

/// A live direct connection to a peer.
pub struct PeerLink {
    conn: Connection,
}

impl PeerLink {
    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// One bi-stream ping/pong to prove the link end-to-end. Dialer sends
    /// `ping` and expects `pong`; the acceptor answers. Used by the debug
    /// connect to confirm a punched link actually carries data.
    pub async fn verify_roundtrip(&self, dialer: bool) -> Result<()> {
        if dialer {
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
    let ep = endpoint()?;
    let key = disco_key(&peer)?;
    let chan = key.channel();

    // Route this session's pokes and listen for the peer's offer before we
    // announce ourselves, so nothing races ahead of the registration.
    let (poke_tx, poke_rx) = mpsc::unbounded_channel();
    ep.sessions.lock().insert(chan, poke_tx);
    let mut offers = signal::listen(peer);

    let result = run_session(ep, key, poke_rx, &mut offers, peer).await;

    ep.sessions.lock().remove(&chan);
    signal::stop(peer);
    result
}

async fn run_session(
    ep: &'static P2pEndpoint,
    key: DiscoKey,
    mut poke_rx: mpsc::UnboundedReceiver<Poke>,
    offers: &mut mpsc::UnboundedReceiver<Vec<SocketAddr>>,
    peer: [u8; 32],
) -> Result<PeerLink> {
    // Publish our candidates, wait for theirs.
    signal::send_offer(peer, candidate::local_candidates(ep.port)).await?;
    let peer_cands = timeout(SIGNAL_TIMEOUT, offers.recv())
        .await
        .map_err(|_| anyhow!("timed out waiting for peer candidates"))?
        .ok_or_else(|| anyhow!("candidate listener closed"))?;

    let our_ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();

    if our_ipk < peer {
        // Dialer: punch, then connect to the address that answered.
        let addr = punch::punch(&ep.pokes, &mut poke_rx, key, peer_cands, PUNCH_TIMEOUT)
            .await
            .ok_or_else(|| anyhow!("hole-punch failed"))?;
        let conn = ep.endpoint.connect(addr, PEER_SNI)?.await?;
        Ok(PeerLink { conn })
    } else {
        // Acceptor: run the punch in the background purely to open our NAT
        // (its validation result doesn't matter — the hole is what counts),
        // and accept the dialer's connection.
        let pokes = ep.pokes.clone();
        let engine = RUNTIME.spawn(async move {
            let mut rx = poke_rx;
            let _ = punch::punch(&pokes, &mut rx, key, peer_cands, PUNCH_TIMEOUT).await;
        });
        let incoming = timeout(PUNCH_TIMEOUT, ep.endpoint.accept())
            .await
            .map_err(|_| anyhow!("timed out waiting for inbound connection"))?
            .ok_or_else(|| anyhow!("endpoint closed"))?;
        // ponytail: MVP accepts the first inbound. Only this peer knows our
        // punched address (we sent candidates over E2E MLS), and peer TLS
        // gates on a valid IPK cert — but the real filter is matching the
        // accepted connection's IPK to `peer`; add when >1 concurrent
        // session is possible.
        let conn = incoming.accept()?.await?;
        engine.abort();
        Ok(PeerLink { conn })
    }
}
