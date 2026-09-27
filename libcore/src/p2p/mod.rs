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
//!
//! Every attempt is a session with its own id and a short life. Offers
//! name the attempt they belong to, so a session pairs only with the reply
//! to its own offer and a stale offer a relay held for a peer who was away
//! is dropped, never answered.

#![allow(dead_code)]

pub(crate) mod candidate;
pub(crate) mod consent;
pub(crate) mod diagnostics;
pub(crate) mod protocol;
mod disco;
mod punch;
mod reflexive;
mod signal;
mod socket;
mod tcp;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::p2p_relay::RelayMsg;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use quinn::Connection;
use quinn::Endpoint;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::RUNTIME;
use crate::data::identity::Identity;
use crate::utils::addr_short;
use crate::utils::addrs_short;
use disco::DiscoKey;
use socket::Poke;
use socket::PokeSender;
use socket::TurnRoutes;
use diagnostics::Event;

/// Inbound P2P candidate offer, routed from the MLS dispatch
/// (`quic/server.rs`) to the session waiting for that peer.
pub(crate) use signal::Offer;
pub(crate) use signal::deliver as deliver_offer;
#[cfg(test)]
pub(crate) use socket::test_tcp_endpoint;

/// How long an offer is worth answering: the life the relay gives the
/// dispatch, and the expiry the receiver checks on the sealed payload. Past
/// it the bridge and the secrets in the offer belong to a session that has
/// already given up: a dialer waits [`DIAL_TIMEOUT`], an acceptor
/// [`SIGNAL_TIMEOUT`] then [`ACCEPT_TIMEOUT`], and the rest is relay hops.
pub(crate) const OFFER_TTL_MS: u64 = 15_000;

/// TLS SNI for peer connections. The peer verifier checks neither the name
/// nor an issuer — identity is settled per stream by
/// [`crate::transfer::auth`], which pins the peer's IPK to the TLS sub-key
/// this connection presented — so any stable string does.
const PEER_SNI: &str = "peer";
/// Wait this long for the peer's offer. An online peer answers in a second
/// or two; one that is frozen never will, and a tap should learn that
/// quickly rather than sit on a spinner.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(8);
/// The QUIC handshake into the bridge. quinn would wait out its idle timer
/// on a bridge nobody joined; this caps it so a dead relay fails fast.
const DIAL_TIMEOUT: Duration = Duration::from_secs(8);
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The acceptor waits this long for the inbound connection. The dialer
/// connects over the bridge as soon as its offer is out, so the inbound
/// lands about one relay round trip after our `TurnAlloc`.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(8);
const VERIFY_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a session delays its offer waiting for the reflexive probe, so
/// the offer can carry the reflexive candidate. Immediate once probed.
const REFLEXIVE_WAIT: Duration = Duration::from_millis(600);
/// A reflexive address older than this is probed again before an offer
/// carries it: the NAT mapping moves with the network.
const REFLEXIVE_MAX_AGE: Duration = Duration::from_secs(60);
/// Inbound connections held for a session that has not registered its expected
/// sources yet — the dialer can beat the acceptor to the punch by a round trip.
/// Past this, the oldest is refused.
const INBOUND_BACKLOG: usize = 8;
/// How long an unclaimed inbound connection stays in the backlog.
const INBOUND_BACKLOG_TTL: Duration = Duration::from_secs(10);

/// Whether a session may publish our addresses. Direct needs a local decision —
/// a peer's offer alone never earns it, or any contact could harvest our public
/// and LAN addresses by sending one.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Disclosure {
    Direct,
    RelayOnly,
}

/// Peers we're mid-connect to. Guards against a second session (e.g. the
/// auto-accept below) racing a button-initiated one for the same peer.
static CONNECTING: Lazy<Mutex<HashSet<[u8; 32]>>> = Lazy::new(|| Mutex::new(HashSet::new()));

/// Cancellation (network change, timeout or caller drop) must release the
/// single-flight slot just as an ordinary return does.
struct ConnectingGuard([u8; 32]);
impl ConnectingGuard {
    fn acquire(peer: [u8; 32]) -> Result<Self> {
        if !CONNECTING.lock().insert(peer) { bail!("{ALREADY_CONNECTING}"); }
        Ok(Self(peer))
    }
}
impl Drop for ConnectingGuard {
    fn drop(&mut self) {
        CONNECTING.lock().remove(&self.0);
    }
}

/// Disco channel → the session waiting on pokes for it. The receive loop
/// routes each inbound poke to the right session by its channel tag.
type Sessions = Arc<Mutex<HashMap<[u8; 8], mpsc::Sender<Poke>>>>;

/// Routes each inbound connection to the session expecting it, by source
/// address. A session registers the addresses only its peer can produce (the
/// TURN bridge's synthetic address, or the peer's advertised candidates), so an
/// unrelated dialer can never land in that peer's link slot. Everything
/// unclaimed is refused, keeping quinn's `Incoming` queue from growing on
/// unauthenticated UDP.
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
    /// The latest STUN echo from a relay, published by the socket's
    /// receive loop. A session probes when it is missing or old.
    reflexive: Arc<Mutex<reflexive::Reflexive>>,
    /// Live links keyed by peer IPK, so signaling and transfer reuse one
    /// connection instead of re-dialing. See [`link`].
    links: Mutex<LinkState>,
    /// In-flight sessions subscribe before starting. Network changes cancel
    /// them, while the locked generation prevents late link publication.
    network: watch::Sender<u64>,
    /// Where the permanent acceptor delivers each inbound connection.
    inbound: Arc<Mutex<InboundRouter>>,
}

static P2P: Lazy<Mutex<Option<Arc<P2pEndpoint>>>> = Lazy::new(|| Mutex::new(None));

/// Build one P2P endpoint for the current network and route each inbound poke
/// to its session. A network change retires the socket; the next attempt binds
/// a fresh one. Must be called from the tokio runtime.
fn endpoint() -> Result<Arc<P2pEndpoint>> {
    let mut current = P2P.lock();
    if let Some(ep) = current.as_ref() { return Ok(ep.clone()); }
    let built = (|| -> Result<P2pEndpoint> {
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
                    let _ = tx.try_send((src, bytes));
                }
            }
        });

        let reflexive = Arc::new(Mutex::new(reflexive::Reflexive::default()));
        let observations = reflexive.clone();
        let mut stun_rx = built.stun_rx;
        RUNTIME.spawn(async move {
            while let Some(reply) = stun_rx.recv().await {
                observations.lock().accept(reply);
            }
        });

        // The listener is always on, so accept() must always be drained —
        // a session only ever picks its own connection out of the router.
        let inbound: Arc<Mutex<InboundRouter>> = Arc::new(Mutex::new(InboundRouter::default()));
        let acceptor = built.endpoint.clone();
        let router = inbound.clone();
        RUNTIME.spawn(async move {
            while let Some(incoming) = acceptor.accept().await {
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

fn now_ms() -> u64 {
    crate::utils::systime().as_millis() as u64
}

/// The relay whose bridge and STUN echo a session may use: the one we are
/// connected to if it said it assists, else the best relay on record that
/// did. If neither advertised UDP assist, our connected, authenticated relay
/// is a tentative TCP candidate. Only its authenticated tunnel Ready grants
/// TCP assist; the old handshake's `assist` flag keeps its UDP-only meaning.
fn assist_relay() -> Option<SocketAddr> {
    let live = {
        let guard = crate::state::RELAY.read();
        guard
            .as_ref()
            .filter(|r| r.connection.as_ref().is_some_and(|c| c.close_reason().is_none()))
            .map(|r| (r.host.to_string(), r.port, r.assist))
    };
    let (host, port) = match live.as_ref().filter(|(_, _, assist)| *assist) {
        Some((host, port, _)) => (host.clone(), *port),
        None => match crate::data::relay::Relay::fetch_assist_capable() {
            Some(r) => (r.host.to_string(), r.port),
            None => {
                let (host, port, _) = live?;
                (host, port)
            },
        },
    };
    let ip: IpAddr = host.parse().ok()?;
    Some(SocketAddr::new(ip, port))
}

/// Our server-reflexive address, probed via the assist relay's STUN echo
/// when we have none or the one we have is old. Peer-independent; a stale
/// mapping self-heals through the punch ping exchange, and TURN covers
/// whatever the reflexive candidate can't.
async fn refresh_reflexive(ep: Arc<P2pEndpoint>) -> Option<SocketAddr> {
    let relay = assist_relay()?;
    if let Some(addr) = ep.reflexive.lock().cached(relay, REFLEXIVE_MAX_AGE) {
        return Some(addr);
    }
    let tx = rand_bytes::<8>();
    let reply = ep.reflexive.lock().begin(tx, relay, REFLEXIVE_WAIT)?;
    struct ProbeGuard(Arc<P2pEndpoint>, [u8; 8]);
    impl Drop for ProbeGuard {
        fn drop(&mut self) { self.0.reflexive.lock().cancel(&self.1); }
    }
    let _guard = ProbeGuard(ep.clone(), tx);
    timeout(REFLEXIVE_WAIT, async {
        ep.pokes.send(relay, &RelayMsg::StunReq { tx }.encode()).await.ok()?;
        reply.await.ok()
    }).await.ok().flatten()
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
struct TurnGuard(Arc<P2pEndpoint>, [u8; 16]);
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
    ep: Arc<P2pEndpoint>, token: [u8; 16], relay: SocketAddr, peer: [u8; 32],
) -> (SocketAddr, RouteGuards) {
    let synth = ep.turn.lock().register(token, relay);
    tcp::start(&ep.turn, synth, token, relay, peer);
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
fn hold_route_while_open(
    ep: Arc<P2pEndpoint>, conn: Connection, guards: Vec<RouteGuards>, tasks: Vec<AbortGuard>,
) {
    RUNTIME.spawn(async move {
        conn.closed().await;
        diagnostics::record(if ep.turn.lock().is_direct(conn.remote_address()) {
            Event::DirectLost
        } else { Event::RelayLost });
        drop(guards);
        drop(tasks);
    });
}

/// Deregisters a session's expected inbound sources when dropped.
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

/// Wait for an inbound connection from one of `addrs` — the only sources this
/// session's peer can dial from. Anything else stays with the router.
fn expect_inbound(
    ep: Arc<P2pEndpoint>, addrs: Vec<SocketAddr>,
) -> (mpsc::UnboundedReceiver<quinn::Incoming>, InboundGuard) {
    let (tx, rx) = mpsc::unbounded_channel();
    ep.inbound.lock().claim(&addrs, tx.clone());
    (rx, InboundGuard { ep, addrs, tx })
}

/// Unroutes a session's pokes when dropped — held by whichever task runs
/// the punch, which can outlive the session. A newer session for the same
/// peer reuses the same chan, so the route is only removed if it is still
/// ours.
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

/// Stops the session's offer listener when dropped — held for as long as
/// the session still reads offers, and no longer, so a later session for
/// the same peer is never unhooked by an old one winding down.
struct OfferGuard {
    peer: [u8; 32],
    tx:   mpsc::UnboundedSender<Offer>,
}
impl Drop for OfferGuard {
    fn drop(&mut self) {
        signal::stop(self.peer, &self.tx);
    }
}

/// Background upgrade: punch, and on a validated address flip the bridge's
/// egress to direct. `set_direct` returning false means the route (and its
/// connection) died first — nothing to upgrade.
async fn punch_upgrade(
    ep: Arc<P2pEndpoint>, mut poke_rx: mpsc::Receiver<Poke>, key: DiscoKey,
    cands: Vec<SocketAddr>, token: [u8; 16], peer: [u8; 32],
) {
    // Accept the peer's datagrams at every address they reach us from, as
    // soon as we hear them. They may flip to direct off their own validated
    // path while ours is still dying in a NAT, and an inbound relabel we
    // only install on our own upgrade would arrive too late (or never).
    let accept_from = |src| {
        ep.turn.lock().accept_from(&token, src);
    };
    match punch::punch(&ep.pokes, &mut poke_rx, key, cands, PUNCH_TIMEOUT, accept_from).await {
        Some(addr) if ep.turn.lock().set_direct(&token, addr) => {
            diagnostics::record(Event::DirectReady);
            log::info!("P2P[{}]: upgraded to direct {}", hex::encode(&peer[..4]), addr_short(addr));
        },
        Some(_) => {},
        None => {
            diagnostics::record(Event::PunchTimeout);
            log::debug!("P2P[{}]: no direct path — staying relayed", hex::encode(&peer[..4]));
        },
    }
}

/// A live direct connection to a peer.
#[derive(Clone)]
pub struct PeerLink {
    pub(crate) conn: Connection,
    dialer: bool,
    disclosure: Disclosure,
    pub ipk: [u8; 32],
}

impl PeerLink {
    /// The immutable TLS-negotiated attachment grammar for this connection.
    /// Transfer authentication still binds the connection's key to `ipk`.
    pub(crate) fn protocol(&self) -> Result<protocol::AttachmentProtocol> {
        protocol::AttachmentProtocol::from_conn(&self.conn)
    }

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
    PeerLink { conn, dialer: false, disclosure: Disclosure::RelayOnly, ipk }
}

/// The bail `connect` emits when a dial to this peer is already in flight;
/// `link` matches on it to wait for the winner instead of failing.
const ALREADY_CONNECTING: &str = "already connecting to that peer";

/// Open a direct connection to `peer`: trade candidates over MLS, punch a
/// hole, then dial (lower IPK) or accept (higher IPK) over the validated
/// address. Both peers call this; the IPK order decides who dials, so
/// exactly one connection forms.
pub async fn connect(peer: [u8; 32]) -> Result<PeerLink> {
    connect_with(peer, Disclosure::Direct, None).await
}

/// [`connect`] with an explicit disclosure decision for this session, and
/// the peer's session this one answers, if it answers one.
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

async fn connect_inner(
    ep: Arc<P2pEndpoint>, generation: u64,
    peer: [u8; 32], disclosure: Disclosure, answering: Option<[u8; 16]>,
) -> Result<PeerLink> {
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
    let chan = channel_for(&our_ipk, &peer);

    // Route this session's pokes and listen for the peer's offer before we
    // announce ourselves, so nothing races ahead of the registration. The
    // channel is public and IPK-derived, so we know it here (the secret key
    // comes with the offer). Each route has a guard: the poke route rides
    // with whichever task runs the punch, the offer listener with whichever
    // still reads offers.
    let (poke_tx, poke_rx) = mpsc::channel(64);
    ep.sessions.lock().insert(chan, poke_tx.clone());
    let poke_guard = PokeGuard { ep: ep.clone(), chan, tx: poke_tx };
    let (offers, offer_tx) = signal::listen(peer);
    let offer_guard = OfferGuard { peer, tx: offer_tx };

    let session = Session {
        ep: ep.clone(),
        peer,
        chan,
        id: rand_bytes::<16>(),
        answering,
        disclosure,
        poke_rx,
        poke_guard,
        offers,
        offer_guard,
    };
    let result = session.run().await;

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
    // Until publication this connection belongs to this attempt. A cancelled
    // verify must close it and release its bridge/upgrade tasks too.
    struct Unpublished(Option<Connection>);
    impl Drop for Unpublished {
        fn drop(&mut self) {
            if let Some(conn) = self.0.take() { conn.close(0u32.into(), b"setup cancelled"); }
        }
    }
    let mut pending = Unpublished(Some(link.conn.clone()));
    if let Err(e) = timeout(VERIFY_TIMEOUT, link.verify_roundtrip()).await
        .map_err(|_| anyhow!("peer verification timed out")).and_then(|r| r)
    {
        diagnostics::record(Event::VerificationFailed);
        log::warn!("P2P[{}]: link verify failed — {e}", hex::encode(&peer[..4]));
        link.conn.close(0u32.into(), b"verify failed");
        return Err(e);
    }
    log::info!(
        "P2P[{}]: connected via {route} — {}",
        hex::encode(&peer[..4]),
        addr_short(link.remote_address())
    );
    if !link.still_permitted() {
        link.conn.close(0u32.into(), b"consent revoked");
        bail!("consent: revoked during connection setup");
    }
    ep.turn.lock().established(link.remote_address());
    ep.links.lock().publish(generation, link.clone())?;
    pending.0 = None;
    diagnostics::record(Event::LinkReady);
    let closed = link.conn.clone();
    RUNTIME.spawn(async move {
        closed.closed().await;
        // An old connection closing must never evict its replacement.
        ep.links.lock().remove_closed(&peer, closed.stable_id());
    });
    // Both ends serve pulls for whatever they retain, so a file offered either
    // direction is fetchable over this one link.
    RUNTIME.spawn(crate::transfer::serve_link(link.clone()));
    // A link the peer opened toward us is the moment to pull whatever we were
    // holding for them: they came online, and this is the connection.
    crate::transfer::on_link_ready(peer);
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
        let mut state = ep.links.lock();
        let links = &mut state.links;
        if let Some(l) = links.get(&peer).cloned() {
            if l.conn.close_reason().is_none() {
                // Re-gate on reuse: a live QUIC link must not outlive consent.
                // Unpaired/forgotten since it opened → sever, don't hand back.
                if l.still_permitted() {
                    return Ok(l);
                }
                links.remove(&peer);
                drop(state);
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
async fn wait_for_cached_link(ep: Arc<P2pEndpoint>, peer: [u8; 32]) -> Result<PeerLink> {
    let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT + DIAL_TIMEOUT + ACCEPT_TIMEOUT;
    loop {
        anyhow::ensure!(*ep.network.borrow() == 0, "network changed while waiting for peer");
        if let Some(l) = ep.links.lock().links.get(&peer).cloned()
            && l.conn.close_reason().is_none() && l.still_permitted()
        {
            return Ok(l);
        }
        // Winner cleared CONNECTING. It publishes the link (connect_inner)
        // before clearing the flag (connect), so once the flag is gone one more
        // cache check is authoritative: present → use it, still absent → the
        // dial failed. Without this re-check a loser that sampled the cache just
        // before the winner's insert-then-clear would bail on a live link.
        if !CONNECTING.lock().contains(&peer) {
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

/// Sever any live direct link to `peer` — the P2P half of the forget/unpair
/// cascade, so a revoked contact's open QUIC connection dies with the pairing.
/// Best-effort: a no-op if the endpoint was never built or no link is open.
pub(crate) fn drop_link(peer: &[u8; 32]) {
    if let Some(ep) = P2P.lock().clone() {
        if let Some(link) = ep.links.lock().links.remove(peer) {
            link.conn.close(0u32.into(), b"contact forgotten");
        }
    }
}

/// Restart attachment transport on the new default network. Existing bridge
/// tokens bind exact source addresses, so reuse would strand a mobile peer.
/// Fresh sessions allocate fresh tokens; the relay's admission policy stays
/// intact. No endpoint is created just because Android reported a network.
pub(crate) async fn network_changed() {
    diagnostics::record(Event::NetworkChanged);
    let retired = P2P.lock().take();
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

/// One connect attempt toward one peer: its identity on the wire, what it
/// may disclose, and the routes it holds open while it runs.
struct Session {
    ep:          Arc<P2pEndpoint>,
    peer:        [u8; 32],
    chan:        [u8; 8],
    id:          [u8; 16],
    /// The peer's session this one answers, if any.
    answering:   Option<[u8; 16]>,
    disclosure:  Disclosure,
    poke_rx:     mpsc::Receiver<Poke>,
    poke_guard:  PokeGuard,
    offers:      mpsc::UnboundedReceiver<Offer>,
    offer_guard: OfferGuard,
}

impl Session {
    fn short(&self) -> String {
        hex::encode(&self.peer[..4])
    }

    /// The next offer from the peer that belongs to this attempt, or `None`
    /// once [`SIGNAL_TIMEOUT`] passes. Offers for other attempts, stale or
    /// answering a session we no longer run, are skipped rather than
    /// consumed: each names a bridge nobody waits on.
    async fn next_offer(&mut self) -> Option<Offer> {
        let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
        loop {
            let offer = tokio::time::timeout_at(deadline, self.offers.recv()).await.ok()??;
            if offer.matches(self.id, self.answering, now_ms()) {
                return Some(offer);
            }
            log::debug!("P2P[{}]: skipping an offer for another attempt", self.short());
        }
    }

    async fn run(mut self) -> Result<(PeerLink, &'static str)> {
        let ep = self.ep.clone();
        let peer = self.peer;
        let disclosure = self.disclosure;
        let direct = disclosure == Disclosure::Direct;
        if !direct { diagnostics::record(Event::RelayOnlyPolicy); }
        let our_ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();

        // Publish our candidates (local + reflexive), the relay whose bridge
        // we can use, and our random session secrets (bridge token + disco
        // key). The bridge, the disco key, and the punch channel are all the
        // dialer's, so the dialer needs nothing back before it connects —
        // only the punch waits on the peer's candidates. A relay-only
        // session publishes the relay alone: its address is the relay's,
        // ours stays private.
        let reflexive = if direct { refresh_reflexive(ep.clone()).await } else { None };
        let our_relay = assist_relay();
        let my_token = rand_bytes::<16>();
        let my_disco_key = rand_bytes::<32>();
        let cands = if direct {
            let mut c = candidate::local_candidates(ep.port);
            c.extend(reflexive);
            c
        } else {
            Vec::new()
        };
        let mine = Offer {
            session:       self.id,
            in_reply_to:   self.answering,
            expires_at_ms: now_ms() + OFFER_TTL_MS,
            candidates:    cands,
            relay:         our_relay,
            token:         my_token,
            disco_key:     my_disco_key,
        };
        if let Err(e) = timeout(SIGNAL_TIMEOUT, signal::send_offer(peer, &mine)).await
            .map_err(|_| anyhow!("sending peer offer timed out")).and_then(|r| r)
        {
            diagnostics::record(Event::SignalingFailed);
            return Err(e);
        }

        let dialer = our_ipk < peer;

        if dialer && let Some(tr) = our_relay {
            // Relay-first: dial the bridge now so the link is usable in about a
            // round trip; the punch runs behind it and upgrades the socket's
            // egress in place when a direct path validates.
            log::info!("P2P[{}]: dialer, connecting via relay bridge", self.short());
            let (synth, guards) = open_turn_route(ep.clone(), my_token, tr, peer);
            let key = DiscoKey::new(&my_disco_key, self.chan);
            let Session { poke_rx, poke_guard, offer_guard, mut offers, id, answering, .. } = self;
            let short = hex::encode(&peer[..4]);
            let upgrade_ep = ep.clone();
            let upgrade = AbortGuard(RUNTIME.spawn(async move {
                let _guards = (poke_guard, offer_guard);
                let deadline = tokio::time::Instant::now() + SIGNAL_TIMEOUT;
                let offer = loop {
                    let Ok(Some(offer)) = tokio::time::timeout_at(deadline, offers.recv()).await
                    else {
                        log::debug!("P2P[{short}]: no peer offer — staying relayed");
                        return;
                    };
                    if !offer.matches(id, answering, now_ms()) {
                        continue;
                    }
                    // The peer started an attempt of their own without seeing
                    // ours (it reached a session they had already retired). Our
                    // bridge is open under this token, so a reply aimed at their
                    // attempt is all they need to join it.
                    if offer.in_reply_to.is_none() && answering.is_none() {
                        let reply = Offer { in_reply_to: Some(offer.session), ..mine.clone() };
                        if let Err(e) = timeout(SIGNAL_TIMEOUT, signal::send_offer(peer, &reply)).await
                            .map_err(|_| anyhow!("crossing peer offer timed out")).and_then(|r| r) {
                            log::debug!("P2P[{short}]: reply to a crossing offer failed — {e}");
                        }
                    }
                    break offer;
                };
                if !direct || offer.candidates.is_empty() {
                    if direct { diagnostics::record(Event::MissingCandidates); }
                    return;
                }
                log::info!(
                    "P2P[{short}]: peer offers [{}], punching in background",
                    addrs_short(&offer.candidates)
                );
                punch_upgrade(upgrade_ep, poke_rx, key, offer.candidates, my_token, peer).await;
            }));
            let conn = timeout(DIAL_TIMEOUT, ep.endpoint.connect(synth, PEER_SNI)?)
                .await
                .map_err(|_| anyhow!("bridge dial timed out after {}s", DIAL_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(Into::into))
                .inspect_err(|_| diagnostics::record(Event::HandshakeFailed))?;
            hold_route_while_open(ep.clone(), conn.clone(), vec![guards], vec![upgrade]);
            return Ok((PeerLink { conn, dialer: true, disclosure, ipk: peer }, "relay"));
        }

        // Both remaining roles need the peer's offer first: the no-relay dialer
        // for the punch targets, the acceptor for the dialer's secrets.
        let offer = self.next_offer().await.ok_or_else(|| {
            diagnostics::record(Event::SignalingFailed);
            anyhow!("timed out waiting for peer candidates")
        })?;
        log::info!(
            "P2P[{}]: {}, peer offers [{}]",
            self.short(),
            if dialer { "dialer (no relay)" } else { "acceptor" },
            addrs_short(&offer.candidates)
        );

        if dialer {
            anyhow::ensure!(direct, "relay required for this peer");
            // No bridge to lean on: the punch is the only path (still serves
            // un-NATed global-IPv6 peers).
            let key = DiscoKey::new(&my_disco_key, self.chan);
            let addr =
                // No bridge in this path, so nothing to relabel: the dial goes
                // straight to whatever the punch validates.
                punch::punch(
                    &ep.pokes,
                    &mut self.poke_rx,
                    key,
                    offer.candidates,
                    PUNCH_TIMEOUT,
                    |_| {},
                )
                .await
                .ok_or_else(|| {
                    diagnostics::record(Event::PunchTimeout);
                    anyhow!("no relay and no direct path")
                })?;
            log::info!("P2P[{}]: hole punched, dialing {}", self.short(), addr_short(addr));
            let conn = timeout(DIAL_TIMEOUT, ep.endpoint.connect(addr, PEER_SNI)?)
                .await
                .map_err(|_| anyhow!("direct dial timed out after {}s", DIAL_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(Into::into))
                .inspect_err(|_| diagnostics::record(Event::HandshakeFailed))?;
            diagnostics::record(Event::DirectReady);
            hold_route_while_open(ep.clone(), conn.clone(), vec![], vec![]);
            return Ok((PeerLink { conn, dialer: true, disclosure, ipk: peer }, "direct"));
        }

        // Acceptor: bridge through the dialer's relay under the dialer's token,
        // and run the punch in the background — it opens our NAT for the
        // dialer's packets, and its validated address upgrades a relayed
        // connection to direct (each side learns the peer's real address from
        // its own pong; no extra signaling).
        let key = DiscoKey::new(&offer.disco_key, self.chan);
        let token = offer.token;
        let mut routes: Vec<RouteGuards> = Vec::new();
        let mut bridged_tokens = HashSet::new();
        let mut sources: Vec<SocketAddr> = Vec::new();
        match offer.relay {
            // Bridged, the dialer's packets reach quinn labelled with the
            // token's synthetic address, and only the holder of that MLS-
            // carried token can produce it.
            Some(tr) => {
                let (synth, guards) = open_turn_route(ep.clone(), token, tr, peer);
                routes.push(guards);
                bridged_tokens.insert(token);
                sources.push(synth);
            },
            // Direct, the dialer arrives from one of its own candidates.
            None if direct => sources.extend(offer.candidates.iter().copied()),
            None => bail!("relay required for this peer"),
        }
        if sources.is_empty() {
            diagnostics::record(Event::MissingCandidates);
            bail!("peer offers neither a relay nor candidates");
        }
        // Registering exactly that set is what keeps someone else's inbound
        // connection out of this peer's link slot.
        let (mut inbound, mut inbound_guard) = expect_inbound(ep.clone(), sources);

        let Session { poke_rx, poke_guard, mut offers, offer_guard, id, answering, .. } = self;
        let short = hex::encode(&peer[..4]);
        let peer_cands = offer.candidates.clone();
        let upgrade_ep = ep.clone();
        let upgrade = AbortGuard(RUNTIME.spawn(async move {
            let _poke_guard = poke_guard;
            if direct && !peer_cands.is_empty() {
                punch_upgrade(upgrade_ep, poke_rx, key, peer_cands, token, peer).await;
            }
        }));
        log::info!("P2P[{short}]: acceptor waiting for inbound");
        let deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
        let incoming = loop {
            tokio::select! {
                got = inbound.recv() => {
                    break got.ok_or_else(|| anyhow!("endpoint closed"))?;
                },
                // The dialer retried while we waited: it dials under a fresh
                // token now, so bridge that one too rather than wait out a
                // bridge it has left.
                got = offers.recv() => {
                    let Some(again) = got else { continue };
                    if !again.matches(id, answering, now_ms()) || again.token == token {
                        continue;
                    }
                    // A retry offer may arrive repeatedly; only a small
                    // bounded set of distinct bridge tokens may own work.
                    if let Some(tr) = again.relay
                        && bridged_tokens.len() < 3 && bridged_tokens.insert(again.token)
                    {
                        let (synth, guards) = open_turn_route(ep.clone(), again.token, tr, peer);
                        routes.push(guards);
                        ep.inbound.lock().claim(&[synth], inbound_guard.tx.clone());
                        inbound_guard.addrs.push(synth);
                        log::info!("P2P[{short}]: dialer retried, bridging its new token too");
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timed out waiting for inbound connection");
                },
            }
        };
        drop(offer_guard);
        // Bound the handshake itself, not just the wait for an inbound. The
        // dialer gives up after DIAL_TIMEOUT, so a handshake still unfinished
        // past ours is one nobody is driving any more — left to quinn's idle
        // timer it holds this peer's CONNECTING slot for another half minute
        // and every retry behind it bails as already-connecting.
        let conn = timeout(ACCEPT_TIMEOUT, incoming.accept()?)
            .await
            .map_err(|_| {
                anyhow!("inbound handshake timed out after {}s", ACCEPT_TIMEOUT.as_secs())
            })??;
        drop(inbound_guard);
        hold_route_while_open(ep.clone(), conn.clone(), routes, vec![upgrade]);
        Ok((PeerLink { conn, dialer: false, disclosure, ipk: peer }, "inbound"))
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_attempt_releases_peer_slot_and_background_work() {
        let peer = [0xe8; 32];
        let (ready, received) = tokio::sync::oneshot::channel();
        let (finished, done) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _attempt = ConnectingGuard::acquire(peer).unwrap();
            let _background = AbortGuard(tokio::spawn(async move {
                let _finished = finished;
                std::future::pending::<()>().await;
            }));
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        received.await.unwrap();
        assert!(ConnectingGuard::acquire(peer).is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(timeout(Duration::from_secs(1), done).await.unwrap().is_err(), "background task leaked");
        assert!(ConnectingGuard::acquire(peer).is_ok(), "cancelled attempt held the peer slot");
    }

    async fn pair() -> (Endpoint, Endpoint, Connection, Connection) {
        use std::net::Ipv4Addr;
        let _ = common::quic::config::setup_crypto_provider();
        let key = ed25519_dalek::SigningKey::from_bytes(&[0xe9; 32]);
        let (server, client) = crate::quic::peer_config::test_peer_configs(&key).unwrap();
        let remote = Endpoint::server(server, (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let mut local = Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        local.set_default_client_config(client);
        let (a, b) = tokio::join!(
            async { local.connect(remote.local_addr().unwrap(), PEER_SNI).unwrap().await.unwrap() },
            async { remote.accept().await.unwrap().await.unwrap() },
        );
        (local, remote, a, b)
    }

    #[tokio::test]
    async fn stale_generation_cannot_publish_or_remove_a_replacement_link() {
        timeout(Duration::from_secs(5), async {
            let (_ep_a, _ep_b, old, _remote_old) = pair().await;
            let (_ep_c, _ep_d, current, _remote_current) = pair().await;
            let peer = [0xea; 32];
            let mut state = LinkState { generation: 0, links: HashMap::new() };
            state.publish(0, test_link(old.clone(), peer)).unwrap();
            state.publish(0, test_link(current.clone(), peer)).unwrap();
            assert!(old.close_reason().is_some(), "replacement left the old connection alive");
            state.generation = 1;
            assert!(state.publish(0, test_link(old.clone(), peer)).is_err());
            assert!(old.close_reason().is_some(), "stale setup must close its own connection");
            state.remove_closed(&peer, old.stable_id());
            assert_eq!(state.links[&peer].conn.stable_id(), current.stable_id());
            assert!(current.close_reason().is_none(), "old cleanup closed the replacement");
            current.close(0u32.into(), b"test done");
            state.remove_closed(&peer, current.stable_id());
            assert!(state.links.is_empty());
        }).await.unwrap();
    }
}
