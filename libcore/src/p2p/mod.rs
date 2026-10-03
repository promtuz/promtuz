//! Peer-to-peer transport: connect through the relay's TURN bridge at once, punch a NAT hole in
//! the background, and move the same connection to raw UDP once a direct path validates.

pub(crate) mod candidate;
pub(crate) mod consent;
pub(crate) mod diagnostics;
pub(crate) mod protocol;
mod disco;
mod punch;
mod reflexive;
mod session;
mod signal;
mod socket;
mod tcp;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::bail;
use parking_lot::Mutex;
use quinn::Connection;
use quinn::Endpoint;
use tokio::sync::mpsc;
use tokio::sync::watch;

use crate::state::core;
use crate::utils::addr_short;
use session::connect_inner;
use socket::Poke;
use socket::PokeSender;
use socket::TurnRoutes;
use diagnostics::Event;

pub(crate) use signal::Offer;
pub(crate) use signal::deliver as deliver_offer;

/// The relay's TTL for an offer and the expiry its receiver checks. Past it the sender gave up.
pub(crate) const OFFER_TTL_MS: u64 = 15_000;

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(8);
/// Caps the bridge handshake, which quinn would otherwise let run to its idle timeout.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(8);
/// Inbound connections held until their session registers; a dialer can be a round trip early.
const INBOUND_BACKLOG: usize = 8;
const INBOUND_BACKLOG_TTL: Duration = Duration::from_secs(10);

/// Whether a session may publish our addresses. A peer's offer alone never earns Direct, or any
/// contact could harvest our public and LAN addresses by sending one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Disclosure {
    Direct,
    RelayOnly,
}

#[derive(Default)]
pub(crate) struct P2p {
    /// Peers with a connect in flight, so a second session cannot race the first.
    connecting:       Mutex<HashSet<[u8; 32]>>,
    /// Built lazily per network, from the tokio runtime; a network change retires it.
    endpoint:         Mutex<Option<Arc<P2pEndpoint>>>,
    offer_listeners:  Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<Offer>>>,
    /// Offers that arrived before their session was listening.
    early_offers:     Mutex<HashMap<[u8; 32], Offer>>,
    relay_discovery:  tokio::sync::Mutex<Option<tokio::time::Instant>>,
    discovery_cursor: AtomicUsize,
    diagnostics:      diagnostics::Diagnostics,
}

struct ConnectingGuard([u8; 32]);
impl ConnectingGuard {
    fn acquire(peer: [u8; 32]) -> Result<Self> {
        if !core().p2p.connecting.lock().insert(peer) { bail!("{ALREADY_CONNECTING}"); }
        Ok(Self(peer))
    }
}
impl Drop for ConnectingGuard {
    fn drop(&mut self) {
        core().p2p.connecting.lock().remove(&self.0);
    }
}

type Sessions = Arc<Mutex<HashMap<[u8; 8], mpsc::Sender<Poke>>>>;

/// Routes inbound connections by source address. A session registers only addresses its peer can
/// produce, so an unrelated dialer never lands in that peer's link slot.
#[derive(Default)]
struct InboundRouter {
    waiting: HashMap<SocketAddr, mpsc::UnboundedSender<quinn::Incoming>>,
    backlog: VecDeque<(Instant, quinn::Incoming)>,
}

impl InboundRouter {
    fn route(&mut self, incoming: quinn::Incoming) {
        if let Some(tx) = self.waiting.get(&reflexive::canonical(incoming.remote_address())) {
            let _ = tx.send(incoming);
            return;
        }
        while self.backlog.front().is_some_and(|(at, _)| at.elapsed() > INBOUND_BACKLOG_TTL) {
            self.expire_oldest();
        }
        while self.backlog.len() >= INBOUND_BACKLOG {
            self.expire_oldest();
        }
        self.backlog.push_back((Instant::now(), incoming));
    }

    fn expire_oldest(&mut self) {
        if let Some((_, stale)) = self.backlog.pop_front() {
            stale.refuse();
        }
    }

    fn claim(&mut self, addrs: &[SocketAddr], tx: mpsc::UnboundedSender<quinn::Incoming>) {
        let addrs: Vec<_> = addrs.iter().copied().map(reflexive::canonical).collect();
        let mut held = std::mem::take(&mut self.backlog);
        while let Some((at, incoming)) = held.pop_front() {
            if at.elapsed() > INBOUND_BACKLOG_TTL {
                incoming.refuse();
            } else if addrs.contains(&reflexive::canonical(incoming.remote_address())) {
                let _ = tx.send(incoming);
            } else {
                self.backlog.push_back((at, incoming));
            }
        }
        for addr in addrs {
            self.waiting.insert(addr, tx.clone());
        }
    }

    fn release(&mut self, addrs: &[SocketAddr], tx: &mpsc::UnboundedSender<quinn::Incoming>) {
        for addr in addrs {
            let addr = reflexive::canonical(*addr);
            if self.waiting.get(&addr).is_some_and(|t| t.same_channel(tx)) {
                self.waiting.remove(&addr);
            }
        }
    }
}

struct LinkState {
    generation: u64,
    links: HashMap<[u8; 32], PeerLink>,
}

impl LinkState {
    fn publish(&mut self, generation: u64, link: PeerLink) -> Result<()> {
        if self.generation != generation {
            link.conn.close(0u32.into(), b"network changed during setup");
            bail!("network changed before peer link publication");
        }
        if let Some(previous) = self.links.insert(link.ipk, link.clone())
            && previous.conn.stable_id() != link.conn.stable_id()
        {
            previous.conn.close(0u32.into(), b"peer connection replaced");
        }
        Ok(())
    }

    fn remove_closed(&mut self, peer: &[u8; 32], connection_id: usize) {
        if self.links.get(peer).is_some_and(|l| l.conn.stable_id() == connection_id) {
            self.links.remove(peer);
        }
    }
}

struct P2pEndpoint {
    endpoint: Endpoint,
    pokes:    PokeSender,
    port:     u16,
    sessions: Sessions,
    turn:     Arc<Mutex<TurnRoutes>>,
    reflexive: Arc<Mutex<reflexive::Reflexive>>,
    links: Mutex<LinkState>,
    /// Network changes cancel subscribed sessions; the generation in `links` stops a late publish.
    network: watch::Sender<u64>,
    inbound: Arc<Mutex<InboundRouter>>,
}

fn endpoint() -> Result<Arc<P2pEndpoint>> {
    let mut current = core().p2p.endpoint.lock();
    if let Some(ep) = current.as_ref() { return Ok(ep.clone()); }
    let built = (|| -> Result<P2pEndpoint> {
        let built = socket::build_endpoint()?;
        let local = built.endpoint.local_addr()?;
        let port = local.port();
        log::info!("P2P: endpoint bound to {}", addr_short(local));
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

        let mut inbox = built.inbox;
        let routes = sessions.clone();
        core().spawn(async move {
            while let Some((src, bytes)) = inbox.recv().await {
                if let Some(chan) = disco::peek_channel(&bytes)
                    && let Some(tx) = routes.lock().get(&chan)
                {
                    let _ = tx.try_send((src, bytes));
                }
            }
        });

        let reflexive = Arc::new(Mutex::new(reflexive::Reflexive::default()));
        let observations = reflexive.clone();
        let mut stun_rx = built.stun_rx;
        core().spawn(async move {
            while let Some(reply) = stun_rx.recv().await {
                observations.lock().accept(reply);
            }
        });

        // accept() is always drained; each session picks only its own connection from the router.
        let inbound: Arc<Mutex<InboundRouter>> = Arc::new(Mutex::new(InboundRouter::default()));
        let acceptor = CloseOnDrop(built.endpoint.clone());
        let router = inbound.clone();
        core().spawn(async move {
            while let Some(incoming) = acceptor.0.accept().await {
                router.lock().route(incoming);
            }
        });

        Ok(P2pEndpoint {
            endpoint: built.endpoint,
            pokes: built.pokes,
            port,
            sessions,
            turn: built.turn,
            reflexive,
            links: Mutex::new(LinkState { generation: 0, links: HashMap::new() }),
            network: watch::channel(0).0,
            inbound,
        })
    })()?;
    let ep = Arc::new(built);
    *current = Some(ep.clone());
    Ok(ep)
}

/// Held by the acceptor task: a shutdown drops it, closing the endpoint and every link on it.
struct CloseOnDrop(Endpoint);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"shutdown");
    }
}

struct AbortGuard(tokio::task::JoinHandle<Option<()>>);
impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct TurnGuard(Arc<P2pEndpoint>, [u8; 16]);
impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.turn.lock().unregister(&self.1);
    }
}

type RouteGuards = (AbortGuard, TurnGuard);

struct InboundGuard {
    ep:    Arc<P2pEndpoint>,
    addrs: Vec<SocketAddr>,
    tx:    mpsc::UnboundedSender<quinn::Incoming>,
}
impl Drop for InboundGuard {
    fn drop(&mut self) {
        self.ep.inbound.lock().release(&self.addrs, &self.tx);
    }
}

/// A newer session for the same peer reuses the chan, so only our own route is removed.
struct PokeGuard {
    ep:   Arc<P2pEndpoint>,
    chan: [u8; 8],
    tx:   mpsc::Sender<Poke>,
}
impl Drop for PokeGuard {
    fn drop(&mut self) {
        let mut sessions = self.ep.sessions.lock();
        if sessions.get(&self.chan).is_some_and(|t| t.same_channel(&self.tx)) {
            sessions.remove(&self.chan);
        }
    }
}

struct OfferGuard {
    peer: [u8; 32],
    tx:   mpsc::UnboundedSender<Offer>,
}
impl Drop for OfferGuard {
    fn drop(&mut self) {
        signal::stop(self.peer, &self.tx);
    }
}

#[derive(Clone)]
pub struct PeerLink {
    pub(crate) conn: Connection,
    dialer: bool,
    disclosure: Disclosure,
    pub ipk: [u8; 32],
}

impl PeerLink {
    fn still_permitted(&self) -> bool {
        match consent::may_connect(&self.ipk) {
            consent::Decision::Direct => true,
            consent::Decision::RelayedOnly => self.disclosure == Disclosure::RelayOnly,
            consent::Decision::No => false,
        }
    }

    pub fn remote_address(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    pub async fn open_stream(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        Ok(self.conn.open_bi().await?)
    }

    pub async fn accept_stream(&self) -> Result<(quinn::SendStream, quinn::RecvStream)> {
        Ok(self.conn.accept_bi().await?)
    }

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

/// A direct loopback connection as a link, without the punch choreography.
#[cfg(test)]
pub(crate) fn test_link(conn: Connection, ipk: [u8; 32]) -> PeerLink {
    PeerLink { conn, dialer: false, disclosure: Disclosure::RelayOnly, ipk }
}

/// `link` matches this error text to wait for the winning dial instead of failing.
const ALREADY_CONNECTING: &str = "already connecting to that peer";

/// Both peers call this; the lower IPK dials and the higher accepts, so one connection forms.
pub async fn connect(peer: [u8; 32]) -> Result<PeerLink> {
    connect_with(peer, Disclosure::Direct, None).await
}

pub async fn connect_with(
    peer: [u8; 32], want: Disclosure, answering: Option<[u8; 16]>,
) -> Result<PeerLink> {
    let disclosure = match consent::may_connect(&peer) {
        consent::Decision::No => bail!("consent: not permitted to connect to that peer"),
        consent::Decision::RelayedOnly => Disclosure::RelayOnly,
        consent::Decision::Direct => want,
    };
    let _guard = ConnectingGuard::acquire(peer)?;
    let ep = endpoint()?;
    let mut network = ep.network.subscribe();
    let generation = *network.borrow_and_update();
    anyhow::ensure!(generation == 0, "network changed before peer connection");
    tokio::select! {
        biased;
        _ = network.changed() => bail!("network changed during peer connection"),
        result = connect_inner(ep, generation, peer, disclosure, answering) => result,
    }
}

pub async fn link(peer: [u8; 32]) -> Result<PeerLink> {
    let ep = endpoint()?;
    // Reuse-or-prune under one lock: a separate get-then-remove could evict a link a concurrent
    // dialer just inserted.
    {
        let mut state = ep.links.lock();
        let links = &mut state.links;
        if let Some(l) = links.get(&peer).cloned() {
            if l.conn.close_reason().is_none() {
                // A live link must not outlive consent.
                if l.still_permitted() {
                    return Ok(l);
                }
                links.remove(&peer);
                drop(state);
                l.conn.close(0u32.into(), b"consent revoked");
                bail!("consent: not permitted to connect to that peer");
            }
            links.remove(&peer);
        }
    }
    match connect(peer).await {
        Ok(l) => Ok(l),
        Err(e) if e.to_string() == ALREADY_CONNECTING => wait_for_cached_link(ep, peer).await,
        Err(e) => Err(e),
    }
}

async fn wait_for_cached_link(ep: Arc<P2pEndpoint>, peer: [u8; 32]) -> Result<PeerLink> {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT + DIAL_TIMEOUT + ACCEPT_TIMEOUT;
    loop {
        anyhow::ensure!(*ep.network.borrow() == 0, "network changed while waiting for peer");
        if let Some(l) = ep.links.lock().links.get(&peer).cloned()
            && l.conn.close_reason().is_none() && l.still_permitted()
        {
            return Ok(l);
        }
        // The winner publishes its link before clearing `connecting`, so once the flag is gone one
        // more cache check is authoritative.
        if !core().p2p.connecting.lock().contains(&peer) {
            if let Some(l) = ep.links.lock().links.get(&peer).cloned()
                && l.conn.close_reason().is_none() && l.still_permitted()
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

pub(crate) fn drop_link(peer: &[u8; 32]) {
    if let Some(ep) = core().p2p.endpoint.lock().clone() {
        if let Some(link) = ep.links.lock().links.remove(peer) {
            link.conn.close(0u32.into(), b"contact forgotten");
        }
    }
}

/// Bridge tokens bind exact source addresses, so the endpoint and its links are retired.
pub(crate) async fn network_changed() {
    diagnostics::record(Event::NetworkChanged);
    let retired = core().p2p.endpoint.lock().take();
    if let Some(ep) = retired {
        let links = {
            let mut state = ep.links.lock();
            state.generation = state.generation.wrapping_add(1);
            ep.reflexive.lock().invalidate();
            ep.network.send_replace(state.generation);
            std::mem::take(&mut state.links)
        };
        ep.endpoint.close(0u32.into(), b"network changed");
        for (_, link) in links {
            link.conn.close(0u32.into(), b"network changed");
        }
    }
    crate::transfer::on_network_changed().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::transfer::handshake;

    #[tokio::test]
    async fn a_cancelled_attempt_releases_its_peer_slot_and_background_work() {
        let _core = crate::test_support::ScopedCore::new();
        let peer: [u8; 32] = rand::random();
        let (ready, started) = tokio::sync::oneshot::channel();
        let (finished, background) = tokio::sync::oneshot::channel::<()>();
        let attempt = tokio::spawn(async move {
            let _slot = ConnectingGuard::acquire(peer).unwrap();
            let _work = AbortGuard(core().spawn(async move {
                let _finished = finished;
                std::future::pending::<()>().await
            }));
            ready.send(()).unwrap();
            std::future::pending::<()>().await
        });
        started.await.unwrap();
        assert!(ConnectingGuard::acquire(peer).is_err(), "one attempt per peer");
        attempt.abort();
        assert!(attempt.await.unwrap_err().is_cancelled());
        assert!(background.await.is_err(), "the background work stopped with it");
        assert!(ConnectingGuard::acquire(peer).is_ok(), "the slot is free again");
    }

    #[tokio::test]
    async fn a_stale_generation_cannot_publish_or_remove_a_replacement_link() {
        let peer = [0xea; 32];
        let link = |h: &crate::test_support::transfer::Handshake| {
            test_link(h.dialed.as_ref().unwrap().clone(), peer)
        };
        let first = handshake().await;
        let second = handshake().await;
        let (old, current) = (link(&first), link(&second));
        let mut state = LinkState { generation: 0, links: HashMap::new() };
        state.publish(0, old.clone()).unwrap();
        state.publish(0, current.clone()).unwrap();
        assert!(old.conn.close_reason().is_some(), "a replacement closes the link it replaces");
        state.generation = 1;
        assert!(state.publish(0, old.clone()).is_err());
        state.remove_closed(&peer, old.conn.stable_id());
        assert_eq!(state.links[&peer].conn.stable_id(), current.conn.stable_id());
        assert!(current.conn.close_reason().is_none(), "the old cleanup leaves the replacement");
        current.conn.close(0u32.into(), b"done");
        state.remove_closed(&peer, current.conn.stable_id());
        assert!(state.links.is_empty());
    }
}
