//! The P2P socket: one UDP port that carries QUIC, our disco pokes, and
//! relay-assist datagrams, so the NAT hole a poke opens is the one the QUIC
//! handshake reuses.
//!
//! On receive it splits them: a datagram that looks like disco
//! ([`disco::peek_channel`]) goes to the punch layer over `inbox`; a
//! relay-assist `TurnData` datagram is unwrapped and presented to quinn as
//! if it came direct from the peer's synthetic address ([`TurnRoutes`]);
//! everything else is a QUIC packet. Pokes and TURN sends go out through
//! [`PokeSender`] / [`AsyncUdpSocket::try_send`] on the same socket.
//!
//! ponytail: naive one-datagram-per-recv, no GSO/GRO — fine for the pokes
//! and the handshake. If bulk device-to-device transfer throughput needs
//! it, back this with `quinn::udp::UdpSocketState` and split GRO batches by
//! stride before the demux.

use std::collections::HashMap;
use std::io;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use anyhow::Result;
use common::proto::p2p_relay::RelayMsg;
use common::quic::tunnel::Channel;
use parking_lot::Mutex;
use quinn::AsyncUdpSocket;
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TokioRuntime;
use quinn::UdpPoller;
use quinn::udp;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::disco;
use crate::quic::peer_config::build_peer_client_cfg;
use crate::quic::peer_config::build_peer_server_cfg;
use crate::quic::peer_identity::PeerIdentity;
use crate::utils::addr_short;

/// Handshakes quinn will hold before it starts refusing. A phone runs at most a
/// couple of concurrent sessions, and [`crate::p2p`] drains `accept()`
/// continuously, so anything past this is unauthenticated UDP piling up.
const MAX_INCOMING: usize = 16;

/// An inbound disco poke: the sender's address and the raw sealed bytes.
pub type Poke = (SocketAddr, Vec<u8>);

/// A relay's STUN echo: the query's tx-id and the public address it saw us
/// from.
pub type StunReply = (SocketAddr, [u8; 8], SocketAddr);

/// Where quinn packets for one synthetic peer address really go: wrapped to
/// the TURN relay (the default), or raw to a punch-validated direct address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Egress {
    Relay { relay: SocketAddr, token: [u8; 16] },
    Tcp,
    Direct { addr: SocketAddr },
}

#[derive(Default)]
struct TcpRoute {
    channel: Option<Arc<Channel>>,
    active: bool,
    established: bool,
    started: bool,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for TcpRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpRoute")
            .field("active", &self.active)
            .field("established", &self.established)
            .finish_non_exhaustive()
    }
}

impl Drop for TcpRoute {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
        if let Some(channel) = &self.channel {
            channel.close();
        }
    }
}

/// Maps between TURN bridge tokens and the synthetic peer addresses quinn
/// uses for them. Shared between the session manager (which registers a
/// bridge and later upgrades it via [`Self::set_direct`]) and the socket
/// (which redirects outbound and relabels inbound datagrams). The synthetic
/// address is a pure quinn-side handle — packets to it are redirected,
/// never sent to it — and it stays the peer's address for the connection's
/// whole life, whichever real path carries the traffic.
#[derive(Debug, Default)]
pub struct TurnRoutes {
    by_synth: HashMap<SocketAddr, Egress>,
    // A delayed offer can register the same bridge while its original link is
    // still alive. Each registration owns one reference, so a timed-out newer
    // attempt cannot remove the first connection's route.
    by_token: HashMap<[u8; 16], (SocketAddr, usize)>,
    /// Peer's punch-validated real address → synth, for the inbound relabel
    /// once a session goes direct.
    by_real: HashMap<SocketAddr, SocketAddr>,
    relays: HashMap<[u8; 16], SocketAddr>,
    tcp: HashMap<SocketAddr, TcpRoute>,
    recv_waker: Option<Waker>,
    recv_cursor: usize,
    next: u32,
}

impl TurnRoutes {
    /// Lease a bridge to `relay` under `token`, returning its shared synthetic
    /// address. Every call must be paired with one `unregister`.
    pub fn register(&mut self, token: [u8; 16], relay: SocketAddr) -> SocketAddr {
        if let Some((synth, owners)) = self.by_token.get_mut(&token) {
            *owners += 1;
            return *synth;
        }
        self.next += 1;
        let n = self.next;
        // A unique address in the RFC 6666 discard prefix (100::/64): never
        // routable, never a real candidate — a pure quinn-side handle.
        let synth = SocketAddr::new(
            Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, (n >> 16) as u16, n as u16).into(),
            9,
        );
        self.by_synth.insert(synth, Egress::Relay { relay, token });
        self.by_token.insert(token, (synth, 1));
        self.relays.insert(token, relay);
        self.tcp.insert(synth, TcpRoute::default());
        synth
    }

    pub fn unregister(&mut self, token: &[u8; 16]) {
        if let Some((_, owners)) = self.by_token.get_mut(token)
            && *owners > 1
        {
            *owners -= 1;
            return;
        }
        if let Some((synth, _)) = self.by_token.remove(token) {
            self.by_synth.remove(&synth);
            self.by_real.retain(|_, s| *s != synth);
            self.relays.remove(token);
            self.tcp.remove(&synth);
        }
    }

    /// Upgrade `token`'s bridge to a punch-validated direct path: egress
    /// flips to raw UDP toward `addr`, and inbound datagrams from `addr` are
    /// relabeled to the synth so quinn never sees the peer's address change.
    /// The relay ingress stays live, so nothing in flight is dropped.
    /// `false` if the route is already gone (its connection died first).
    pub fn set_direct(&mut self, token: &[u8; 16], addr: SocketAddr) -> bool {
        let Some(&(synth, _)) = self.by_token.get(token) else { return false };
        self.by_synth.insert(synth, Egress::Direct { addr });
        self.by_real.insert(super::reflexive::canonical(addr), synth);
        true
    }

    /// Accept inbound datagrams from a peer address the punch has
    /// authenticated, without touching our egress.
    ///
    /// The upgrade is one-sided whenever the punch is: a peer whose pings
    /// reach us validates that path and flips to direct while our own pings
    /// die in their NAT, so we never call [`Self::set_direct`] and keep
    /// egressing through the relay. Their packets then arrive raw from an
    /// address we have no synth for, and quinn drops them as belonging to no
    /// connection — the bridge looks alive from our side and the handshake
    /// never completes. Registering the source repairs the inbound half
    /// alone: they egress direct, we egress relayed, and both ends still see
    /// the one synthetic address. `false` if the route is already gone.
    pub fn accept_from(&mut self, token: &[u8; 16], src: SocketAddr) -> bool {
        let Some(&(synth, _)) = self.by_token.get(token) else { return false };
        self.by_real.insert(super::reflexive::canonical(src), synth);
        true
    }

    /// If `dest` is a synthetic address, where its quinn packets really go.
    fn egress(&self, dest: SocketAddr) -> Option<Egress> {
        match self.by_synth.get(&dest).copied() {
            Some(Egress::Relay { .. }) if self.tcp.get(&dest).is_some_and(|r| r.active) => {
                Some(Egress::Tcp)
            },
            route => route,
        }
    }

    /// The synth for a peer's punch-validated real address, if any — the
    /// inbound half of a direct upgrade.
    fn synth_for_real(&self, src: &SocketAddr) -> Option<SocketAddr> {
        self.by_real.get(&super::reflexive::canonical(*src)).copied()
    }

    /// The synthetic address for an inbound TURN datagram's token, if we
    /// have a session for it.
    fn synth_for(&self, token: &[u8; 16]) -> Option<SocketAddr> {
        self.by_token.get(token).map(|(synth, _)| *synth)
    }

    fn synth_for_relay(&self, token: &[u8; 16], relay: SocketAddr) -> Option<SocketAddr> {
        (self.relays.get(token).map(|a| super::reflexive::canonical(*a))
            == Some(super::reflexive::canonical(relay)))
        .then(|| self.synth_for(token))
        .flatten()
    }

    pub(super) fn begin_tcp(
        &mut self, token: &[u8; 16], synth: SocketAddr, relay: SocketAddr,
    ) -> bool {
        if self.synth_for_relay(token, relay) != Some(synth) {
            return false;
        }
        let Some(route) = self.tcp.get_mut(&synth) else { return false };
        if route.started {
            return false;
        }
        route.started = true;
        true
    }

    pub(super) fn own_tcp_worker(
        &mut self, synth: SocketAddr, worker: tokio::task::JoinHandle<()>,
    ) {
        if let Some(route) = self.tcp.get_mut(&synth) {
            route.worker = Some(worker);
        } else {
            worker.abort();
        }
    }

    pub(super) fn needs_tcp(&self, synth: SocketAddr) -> bool {
        matches!(self.by_synth.get(&synth), Some(Egress::Relay { .. }))
            && self.tcp.get(&synth).is_some_and(|r| !r.established || r.active)
    }

    pub(super) fn install_tcp(&mut self, synth: SocketAddr, channel: Arc<Channel>) -> bool {
        if !self.needs_tcp(synth) {
            channel.close();
            return false;
        }
        let route = self.tcp.get_mut(&synth).unwrap();
        if route.channel.is_some() {
            channel.close();
            return false;
        }
        route.channel = Some(channel);
        if let Some(waker) = self.recv_waker.take() {
            waker.wake();
        }
        true
    }

    pub(super) fn tcp_active(&self, synth: SocketAddr) -> bool {
        self.tcp.get(&synth).is_some_and(|r| r.active)
    }

    pub(super) fn established(&mut self, synth: SocketAddr) {
        if let Some(route) = self.tcp.get_mut(&synth) {
            route.established = true;
        }
    }

    fn tcp_channel(&self, synth: SocketAddr) -> Option<Arc<Channel>> {
        self.tcp.get(&synth)?.channel.clone()
    }

    pub(super) fn remove_tcp(&mut self, synth: SocketAddr, channel: &Arc<Channel>) {
        if let Some(route) = self.tcp.get_mut(&synth)
            && route.channel.as_ref().is_some_and(|c| Arc::ptr_eq(c, channel))
        {
            route.channel.take().unwrap().close();
            if route.active {
                super::diagnostics::record(super::diagnostics::Event::TcpRelayLost);
            }
            route.active = false;
        }
    }

    fn accept_tcp(&mut self, synth: SocketAddr, channel: &Arc<Channel>) -> bool {
        let Some(route) = self.tcp.get_mut(&synth) else { return false };
        if !route.channel.as_ref().is_some_and(|c| Arc::ptr_eq(c, channel)) {
            return false;
        }
        if !route.active {
            super::diagnostics::record(super::diagnostics::Event::TcpRelayReady);
            route.active = true;
        }
        true
    }

    pub(super) fn is_direct(&self, addr: SocketAddr) -> bool {
        matches!(self.egress(addr), Some(Egress::Direct { .. }) | None)
    }
}

/// Sends disco pokes (and relay-assist control) on the P2P socket — the
/// same port quinn uses, so pokes and the QUIC handshake share one NAT
/// mapping.
#[derive(Clone)]
pub struct PokeSender {
    io: Arc<UdpSocket>,
}

impl PokeSender {
    pub async fn send(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        self.io.send_to(bytes, to).await.map(|_| ())
    }
}

/// The custom socket handed to quinn. Peels disco + TURN off the QUIC
/// stream.
#[derive(Debug)]
pub struct PunchSocket {
    io: Arc<UdpSocket>,
    inbox_tx: mpsc::Sender<Poke>,
    stun_tx: mpsc::Sender<StunReply>,
    turn: Arc<Mutex<TurnRoutes>>,
    tcp_first: AtomicBool,
}

/// What one bound P2P socket yields: the socket for quinn, a poke sender,
/// the inbound-poke stream, the relay STUN-echo stream, and the shared TURN
/// routing table.
pub struct Bound {
    pub socket: Arc<PunchSocket>,
    pub pokes: PokeSender,
    pub inbox: mpsc::Receiver<Poke>,
    pub stun_rx: mpsc::Receiver<StunReply>,
    pub turn: Arc<Mutex<TurnRoutes>>,
}

impl PunchSocket {
    /// Bind the P2P UDP socket. Must run inside the tokio runtime — it
    /// registers with the reactor.
    pub fn bind(addr: SocketAddr) -> io::Result<Bound> {
        let std_sock = std::net::UdpSocket::bind(addr)?;
        std_sock.set_nonblocking(true)?;
        let io = Arc::new(UdpSocket::from_std(std_sock)?);
        let (inbox_tx, inbox) = mpsc::channel(128);
        let (stun_tx, stun_rx) = mpsc::channel(32);
        let turn = Arc::new(Mutex::new(TurnRoutes::default()));
        Ok(Bound {
            socket: Arc::new(Self {
                io: io.clone(),
                inbox_tx,
                stun_tx,
                turn: turn.clone(),
                tcp_first: AtomicBool::new(false),
            }),
            pokes: PokeSender { io },
            inbox,
            stun_rx,
            turn,
        })
    }
}

impl AsyncUdpSocket for PunchSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(PokePoller { io: self.io.clone() })
    }

    fn try_send(&self, transmit: &udp::Transmit) -> io::Result<()> {
        // max_transmit_segments defaults to 1, so quinn never sets a GSO
        // segment_size — contents is a single datagram.
        let (egress, tcp) = {
            let routes = self.turn.lock();
            (routes.egress(transmit.destination), routes.tcp_channel(transmit.destination))
        };
        let result = match egress {
            // TURN path: wrap the QUIC datagram so the relay forwards it to
            // the peer under this bridge's token.
            Some(Egress::Relay { relay, token }) => {
                let framed = RelayMsg::TurnData { token, payload: transmit.contents }.encode();
                log::trace!("P2P: TURN send {}B -> {}", transmit.contents.len(), addr_short(relay));
                let udp = self.io.try_send_to(&framed, relay).map(|_| ());
                if udp.is_ok() {
                    super::diagnostics::sent_datagram(true, transmit.contents.len());
                }
                // The authenticated bridge namespace cannot join a legacy UDP
                // bridge. Until TCP ingress proves both peers joined, retain
                // UDP egress too; a new client can still reach an old peer.
                let tcp = tcp.map(|channel| channel.try_send(transmit.contents));
                if tcp.as_ref().is_some_and(|result| result.is_ok()) {
                    super::diagnostics::sent_datagram(true, transmit.contents.len());
                    return Ok(());
                }
                return udp;
            },
            Some(Egress::Tcp) => match tcp {
                Some(channel) => match channel.try_send(transmit.contents) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        // Quinn gives this multi-peer socket no destination
                        // when polling writability. Waiting on one TCP queue
                        // would stall unrelated peer connections, so shed the
                        // datagram like UDP packet loss; QUIC owns retransmit
                        // and congestion recovery. The queue remains bounded.
                        super::diagnostics::dropped_tcp_datagram();
                        return Ok(());
                    },
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        self.turn.lock().remove_tcp(transmit.destination, &channel);
                        // Failure belongs to one route, not the shared peer
                        // endpoint. Drop this packet; QUIC recovery/reconnect
                        // will retry through the remaining route.
                        return Ok(());
                    },
                    result => result,
                },
                None => Err(io::Error::new(io::ErrorKind::NotConnected, "TCP route retired")),
            },
            // Upgraded: same synth for quinn, raw UDP underneath.
            Some(Egress::Direct { addr }) => {
                self.io.try_send_to(transmit.contents, addr).map(|_| ())
            },
            None => self.io.try_send_to(transmit.contents, transmit.destination).map(|_| ()),
        };
        if result.is_ok() {
            super::diagnostics::sent_datagram(
                matches!(egress, Some(Egress::Relay { .. } | Egress::Tcp)),
                transmit.contents.len(),
            );
        }
        result
    }

    fn poll_recv(
        &self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.turn.lock().recv_waker = Some(cx.waker().clone());
        // Alternate ready sources so neither bulk TCP nor UDP can starve the
        // other. Every new channel installation wakes this receive poll.
        let tcp_first = self.tcp_first.fetch_xor(true, Ordering::Relaxed);
        if tcp_first && let Poll::Ready(result) = self.poll_tcp(cx, bufs, meta) {
            return Poll::Ready(result);
        }
        if let Poll::Ready(result) = self.poll_udp(cx, bufs, meta) {
            return Poll::Ready(result);
        }
        if !tcp_first {
            return self.poll_tcp(cx, bufs, meta);
        }
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }
}

impl PunchSocket {
    fn poll_tcp(
        &self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let channels = {
            let mut routes = self.turn.lock();
            let mut channels: Vec<_> = routes
                .tcp
                .iter()
                .filter_map(|(&synth, route)| route.channel.clone().map(|c| (synth, c)))
                .collect();
            if !channels.is_empty() {
                let offset = routes.recv_cursor % channels.len();
                channels.rotate_left(offset);
                routes.recv_cursor = routes.recv_cursor.wrapping_add(1);
            }
            channels
        };
        for (synth, channel) in channels {
            match channel.poll_recv(cx) {
                Poll::Ready(Ok(packet)) => {
                    if packet.is_empty() || packet.len() > bufs[0].len() {
                        cx.waker().wake_by_ref();
                        continue;
                    }
                    if !self.turn.lock().accept_tcp(synth, &channel) {
                        continue;
                    }
                    bufs[0][..packet.len()].copy_from_slice(&packet);
                    meta[0] = udp::RecvMeta {
                        addr: synth,
                        len: packet.len(),
                        stride: packet.len(),
                        ecn: None,
                        dst_ip: None,
                    };
                    return Poll::Ready(Ok(1));
                },
                Poll::Ready(Err(_)) => self.turn.lock().remove_tcp(synth, &channel),
                Poll::Pending => {},
            }
        }
        Poll::Pending
    }

    fn poll_udp(
        &self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Drain disco + TURN; return on the first real QUIC datagram (or
        // Pending).
        for _ in 0..64 {
            let (len, src) = {
                let mut rb = tokio::io::ReadBuf::new(&mut bufs[0]);
                match self.io.poll_recv_from(cx, &mut rb) {
                    Poll::Ready(Ok(src)) => (rb.filled().len(), src),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            };
            if disco::peek_channel(&bufs[0][..len]).is_some() {
                // A poke — hand it to the punch layer, keep it from quinn.
                let _ = self.inbox_tx.try_send((src, bufs[0][..len].to_vec()));
                continue;
            }
            // Relay-assist? Only TURN data is a QUIC datagram bound for
            // quinn. Extract just Copy data so bufs[0]'s borrow ends before
            // we may rewrite it in place.
            let turn = match RelayMsg::decode(&bufs[0][..len]) {
                Some(RelayMsg::TurnData { token, payload }) => Some((token, payload.len())),
                Some(RelayMsg::StunResp { tx, seen }) => {
                    let _ = self.stun_tx.try_send((src, tx, seen));
                    continue;
                },
                Some(_) => continue, // StunReq/TurnAlloc — never sent to a client
                None => None,
            };
            if let Some((token, plen)) = turn {
                let Some(synth) = self.turn.lock().synth_for_relay(&token, src) else { continue };
                // Present the bridged QUIC payload to quinn as if it came
                // direct from the peer's synthetic address.
                let off = len - plen;
                bufs[0].copy_within(off..len, 0);
                meta[0] =
                    udp::RecvMeta { addr: synth, len: plen, stride: plen, ecn: None, dst_ip: None };
                return Poll::Ready(Ok(1));
            }
            // A direct datagram from an upgraded session's peer: same synth
            // relabel, so quinn sees one unchanging address either way.
            if let Some(synth) = self.turn.lock().synth_for_real(&src) {
                meta[0] = udp::RecvMeta { addr: synth, len, stride: len, ecn: None, dst_ip: None };
                return Poll::Ready(Ok(1));
            }
            meta[0] = udp::RecvMeta { addr: src, len, stride: len, ecn: None, dst_ip: None };
            return Poll::Ready(Ok(1));
        }
        // Yield even under a continuous stream of assist/poke datagrams.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Registers write-readiness for quinn after a `try_send` WouldBlock.
#[derive(Debug)]
struct PokePoller {
    io: Arc<UdpSocket>,
}

impl UdpPoller for PokePoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.io.poll_send_ready(cx)
    }
}

/// A freshly built P2P endpoint and the handles the session manager needs.
pub struct BuiltEndpoint {
    pub endpoint: Endpoint,
    pub pokes: PokeSender,
    pub inbox: mpsc::Receiver<Poke>,
    pub stun_rx: mpsc::Receiver<StunReply>,
    pub turn: Arc<Mutex<TurnRoutes>>,
}

/// Build the P2P endpoint on a fresh punch socket. Client and server
/// configs are both the self-signed peer identity — we dial some peers
/// and accept others on the one endpoint. `grease_quic_bit(false)` lets a
/// stray poke be dropped rather than mis-parsed as QUIC.
pub fn build_endpoint() -> Result<BuiltEndpoint> {
    let bound = PunchSocket::bind((Ipv6Addr::UNSPECIFIED, 0).into())?;
    let identity = PeerIdentity::initialize()?;

    let mut ep_cfg = EndpointConfig::default();
    ep_cfg.grease_quic_bit(false);

    let mut server_cfg = build_peer_server_cfg(&identity)?;
    server_cfg.max_incoming(MAX_INCOMING);

    let mut endpoint = Endpoint::new_with_abstract_socket(
        ep_cfg,
        Some(server_cfg),
        bound.socket,
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(build_peer_client_cfg(&identity)?);
    Ok(BuiltEndpoint {
        endpoint,
        pokes: bound.pokes,
        inbox: bound.inbox,
        stun_rx: bound.stun_rx,
        turn: bound.turn,
    })
}

/// Attachment integration tests supply their real authenticated outer pipe
/// and identity keys, then exercise the production peer socket and ALPNs.
#[cfg(test)]
pub(crate) fn test_tcp_endpoint(
    channel: Arc<Channel>, key: &ed25519_dalek::SigningKey, relay: SocketAddr, token: [u8; 16],
) -> Result<(Endpoint, SocketAddr)> {
    let bound = PunchSocket::bind((Ipv6Addr::LOCALHOST, 0).into())?;
    let synth = bound.turn.lock().register(token, relay);
    anyhow::ensure!(bound.turn.lock().install_tcp(synth, channel), "test TCP route failed");
    let (server, client) = crate::quic::peer_config::test_peer_configs_with_protocols(
        key,
        super::protocol::offered_alpns(),
    )?;
    let mut config = EndpointConfig::default();
    config.grease_quic_bit(false);
    let mut endpoint = Endpoint::new_with_abstract_socket(
        config,
        Some(server),
        bound.socket,
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(client);
    Ok((endpoint, synth))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;

    fn empty_meta() -> udp::RecvMeta {
        udp::RecvMeta {
            addr: (Ipv4Addr::UNSPECIFIED, 0).into(),
            len: 0,
            stride: 0,
            ecn: None,
            dst_ip: None,
        }
    }

    #[test]
    fn turn_route_maps_token_and_synth() {
        let mut r = TurnRoutes::default();
        let relay: SocketAddr = "9.9.9.9:443".parse().unwrap();
        let s1 = r.register([1; 16], relay);
        let s2 = r.register([2; 16], relay);
        assert_ne!(s1, s2); // distinct synthetic address per token
        assert_eq!(r.register([1; 16], relay), s1); // idempotent
        assert_eq!(r.egress(s1), Some(Egress::Relay { relay, token: [1; 16] }));
        assert_eq!(r.synth_for(&[1; 16]), Some(s1));
        // a real (non-synthetic) address passes straight through
        assert_eq!(r.egress("1.2.3.4:5".parse().unwrap()), None);
        r.unregister(&[1; 16]);
        assert_eq!(r.synth_for(&[1; 16]), Some(s1), "the other route owner must survive");
        r.unregister(&[1; 16]);
        assert_eq!(r.egress(s1), None);
        assert_eq!(r.synth_for(&[1; 16]), None);
    }

    #[test]
    fn relay_ingress_requires_the_registered_source_even_after_direct_upgrade() {
        let mut routes = TurnRoutes::default();
        let relay: SocketAddr = "203.0.113.1:443".parse().unwrap();
        let unrelated: SocketAddr = "203.0.113.2:443".parse().unwrap();
        let token = [81; 16];
        let synth = routes.register(token, relay);
        assert_eq!(routes.synth_for_relay(&token, unrelated), None);
        assert_eq!(routes.synth_for_relay(&token, relay), Some(synth));
        assert_eq!(
            routes.synth_for_relay(&token, "[::ffff:203.0.113.1]:443".parse().unwrap()),
            Some(synth)
        );
        routes.set_direct(&token, "198.51.100.9:5080".parse().unwrap());
        assert_eq!(routes.synth_for_relay(&token, unrelated), None);
        assert_eq!(routes.synth_for_relay(&token, relay), Some(synth));
    }

    #[tokio::test]
    async fn final_route_lease_cancels_setup_and_old_synth_cannot_replace_new_route() {
        let mut routes = TurnRoutes::default();
        let relay: SocketAddr = "203.0.113.3:443".parse().unwrap();
        let token = [82; 16];
        let synth = routes.register(token, relay);
        assert!(routes.begin_tcp(&token, synth, relay));
        assert!(!routes.begin_tcp(&token, synth, relay));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (alive_tx, alive_rx) = tokio::sync::oneshot::channel::<()>();
        routes.own_tcp_worker(
            synth,
            tokio::spawn(async move {
                let _alive = alive_tx;
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            }),
        );
        started_rx.await.unwrap();
        assert_eq!(routes.register(token, relay), synth);
        routes.unregister(&token);
        assert!(routes.needs_tcp(synth), "one lease must retain setup");
        routes.unregister(&token);
        tokio::time::timeout(Duration::from_secs(1), alive_rx).await.unwrap().unwrap_err();
        let replacement = routes.register(token, relay);
        assert_ne!(replacement, synth);
        assert!(!routes.begin_tcp(&token, synth, relay));
        assert!(routes.begin_tcp(&token, replacement, relay));
        routes.established(synth);
        assert!(routes.needs_tcp(replacement), "old completion cannot suppress new setup");
        routes.established(replacement);
        assert!(!routes.needs_tcp(replacement), "verified UDP link does not need TCP setup");
    }

    #[test]
    fn set_direct_flips_egress_keeps_relay_ingress() {
        let mut r = TurnRoutes::default();
        let relay: SocketAddr = "9.9.9.9:443".parse().unwrap();
        let direct: SocketAddr = "1.2.3.4:5000".parse().unwrap();
        let synth = r.register([1; 16], relay);

        assert!(r.set_direct(&[1; 16], direct));
        // egress goes raw to the validated address...
        assert_eq!(r.egress(synth), Some(Egress::Direct { addr: direct }));
        // ...inbound direct datagrams relabel to the synth...
        assert_eq!(r.synth_for_real(&direct), Some(synth));
        // ...and relay-wrapped inbound still relabels too (both paths live).
        assert_eq!(r.synth_for(&[1; 16]), Some(synth));

        // teardown clears the reverse map with the rest
        r.unregister(&[1; 16]);
        assert_eq!(r.synth_for_real(&direct), None);
        assert_eq!(r.egress(synth), None);

        // a dead route can't be upgraded
        assert!(!r.set_direct(&[1; 16], direct));
    }

    /// A poke reaches the punch inbox; a non-poke surfaces to quinn. Runs
    /// over real loopback sockets, driving `poll_recv` the way quinn does.
    #[tokio::test]
    async fn demux_splits_disco_from_quic() {
        let b = PunchSocket::bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let b_addr = b.socket.local_addr().unwrap();
        let sock_b = b.socket;
        let mut inbox = b.inbox;

        let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let a_addr = a.local_addr().unwrap();

        // Stand in for quinn's endpoint driver: poll poll_recv, forward
        // whatever it surfaces as QUIC.
        let (quic_tx, mut quic_rx) = mpsc::unbounded_channel();
        let driver = tokio::spawn(async move {
            let mut store = [0u8; 2048];
            loop {
                let mut bufs = [io::IoSliceMut::new(&mut store)];
                let mut meta = [empty_meta()];
                match std::future::poll_fn(|cx| sock_b.poll_recv(cx, &mut bufs, &mut meta)).await {
                    Ok(_) => {
                        let _ = quic_tx.send((meta[0].addr, bufs[0][..meta[0].len].to_vec()));
                    },
                    Err(_) => break,
                }
            }
        });

        // Disco-shaped → punch inbox, never quinn.
        let poke =
            disco::DiscoKey::new(&[3u8; 32], [4; 8]).seal(&disco::DiscoMsg::Ping { tx: [1; 8] });
        a.send_to(&poke, b_addr).await.unwrap();
        let (src, got) = tokio::time::timeout(Duration::from_secs(1), inbox.recv())
            .await
            .expect("poke not demuxed")
            .unwrap();
        assert_eq!(src, a_addr);
        assert_eq!(got, poke);

        // Non-disco (QUIC fixed-bit set) → surfaces to quinn.
        a.send_to(b"\xc0quic-ish", b_addr).await.unwrap();
        let (src, got) = tokio::time::timeout(Duration::from_secs(1), quic_rx.recv())
            .await
            .expect("quic datagram dropped")
            .unwrap();
        assert_eq!(src, a_addr);
        assert_eq!(&got, b"\xc0quic-ish");

        driver.abort();
    }

    /// A copied transaction/address cannot hide the datagram's real source.
    /// Correlation happens above the socket, so preserve all three fields.
    #[tokio::test]
    async fn demux_preserves_reflexive_response_source() {
        let mut bound = PunchSocket::bind((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
        let dest = bound.socket.local_addr().unwrap();
        let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let unrelated = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let tx = [19; 8];
        let seen: SocketAddr = "9.9.9.9:5100".parse().unwrap();
        let packet = RelayMsg::StunResp { tx, seen }.encode();

        for source in [&relay, &unrelated] {
            source.send_to(&packet, dest).await.unwrap();
            let mut store = [0u8; 2048];
            let mut bufs = [io::IoSliceMut::new(&mut store)];
            let mut meta = [empty_meta()];
            let reply = tokio::time::timeout(Duration::from_secs(1), async {
                tokio::select! {
                    reply = bound.stun_rx.recv() => reply.expect("STUN channel closed"),
                    unexpected = std::future::poll_fn(|cx| {
                        bound.socket.poll_recv(cx, &mut bufs, &mut meta)
                    }) => panic!("STUN response reached QUIC: {unexpected:?}"),
                }
            })
            .await
            .expect("STUN response not demuxed");
            assert_eq!(reply, (source.local_addr().unwrap(), tx, seen));
            assert!(bound.inbox.try_recv().is_err(), "STUN response reached disco inbox");
        }
    }

    /// Stub relay: forward each TurnData to the other source seen under its
    /// token — the minimal version of the real bridge.
    async fn stub_relay() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let relay = Arc::new(UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap());
        let relay_addr = relay.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut buf = vec![0u8; 1600];
            let mut ends: HashMap<[u8; 16], Vec<SocketAddr>> = HashMap::new();
            while let Ok((n, src)) = relay.recv_from(&mut buf).await {
                let (token, is_data) = match RelayMsg::decode(&buf[..n]) {
                    Some(RelayMsg::TurnAlloc { token }) => (token, false),
                    Some(RelayMsg::TurnData { token, .. }) => (token, true),
                    _ => continue,
                };
                let list = ends.entry(token).or_default();
                if !list.contains(&src) && list.len() < 2 {
                    list.push(src);
                }
                if !list.contains(&src) {
                    continue;
                }
                if is_data && let Some(&dst) = list.iter().find(|&&a| a != src) {
                    let _ = relay.send_to(&buf[..n], dst).await;
                }
            }
        });
        (relay_addr, task)
    }

    /// A peer endpoint on a fresh punch socket (throwaway key — the verifier
    /// accepts any valid self-signed Ed25519 cert).
    fn peer_endpoint() -> (Endpoint, PokeSender, Arc<Mutex<TurnRoutes>>) {
        let (endpoint, pokes, routes, _) = peer_endpoint_with_socket();
        (endpoint, pokes, routes)
    }

    fn peer_endpoint_with_socket()
    -> (Endpoint, PokeSender, Arc<Mutex<TurnRoutes>>, Arc<PunchSocket>) {
        use ed25519_dalek::SigningKey;

        use crate::quic::peer_config::test_peer_configs;

        let key = SigningKey::from_bytes(&[7u8; 32]);
        let bound = PunchSocket::bind((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        let (server_cfg, client_cfg) = test_peer_configs(&key).unwrap();
        let mut ep_cfg = EndpointConfig::default();
        ep_cfg.grease_quic_bit(false);
        let mut ep = Endpoint::new_with_abstract_socket(
            ep_cfg,
            Some(server_cfg),
            bound.socket.clone(),
            Arc::new(TokioRuntime),
        )
        .unwrap();
        ep.set_default_client_config(client_cfg);
        (ep, bound.pokes, bound.turn, bound.socket)
    }

    async fn roundtrip(a: &quinn::Connection, b: &quinn::Connection, msg: &[u8]) {
        let (mut send, mut recv) = a.open_bi().await.unwrap();
        send.write_all(msg).await.unwrap();
        send.finish().unwrap();
        let (mut bsend, mut brecv) = b.accept_bi().await.unwrap();
        assert_eq!(brecv.read_to_end(64).await.unwrap(), msg);
        bsend.write_all(b"ack").await.unwrap();
        bsend.finish().unwrap();
        assert_eq!(recv.read_to_end(64).await.unwrap(), b"ack");
    }

    /// Real TLS outer pipes carry real end-to-end QUIC. The UDP socket at
    /// the relay address discards every packet, so it cannot accidentally
    /// make the test pass via the legacy bearer bridge.
    #[tokio::test]
    async fn peer_quic_and_bytes_cross_one_authenticated_tcp_relay_without_udp_forwarding() {
        use common::quic::tunnel::{self, AcceptedMode, Request};
        use ed25519_dalek::{Signer, SigningKey};

        let _ = common::quic::config::setup_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["relay.test".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let mut tls =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(
                    vec![cert.cert.der().clone()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der())
                        .into(),
                )
                .unwrap();
        tls.alpn_protocols = vec![tunnel::ALPN.to_vec()];
        let tls = Arc::new(tls);
        let listener = tokio::net::TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
        let relay = listener.local_addr().unwrap();
        let blackhole = UdpSocket::bind(relay).await.unwrap();
        let udp_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = udp_seen.clone();
        let blackhole_task = tokio::spawn(async move {
            let mut packet = [0u8; 4096];
            while blackhole.recv_from(&mut packet).await.is_ok() {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        });
        let key_a = SigningKey::from_bytes(&[91; 32]);
        let key_b = SigningKey::from_bytes(&[92; 32]);
        let ipk_a = key_a.verifying_key().to_bytes();
        let ipk_b = key_b.verifying_key().to_bytes();
        let token = [93; 16];
        let relay_task = tokio::spawn(async move {
            let first = tunnel::accept(
                listener.accept().await.unwrap().0,
                tls.clone(),
                tunnel::FEATURE_ASSIST,
            )
            .await
            .unwrap();
            let second =
                tunnel::accept(listener.accept().await.unwrap().0, tls, tunnel::FEATURE_ASSIST)
                    .await
                    .unwrap();
            assert_eq!(first.mode, AcceptedMode::Assist { token, ipk: ipk_a, peer: ipk_b });
            assert_eq!(second.mode, AcceptedMode::Assist { token, ipk: ipk_b, peer: ipk_a });
            let a = first.channel;
            let b = second.channel;
            loop {
                let (packet, target) = tokio::select! {
                    packet = a.recv() => (packet, &b),
                    packet = b.recv() => (packet, &a),
                };
                let Ok(packet) = packet else { break };
                let mut writable = target.clone().create_io_poller();
                loop {
                    match target.try_send(&packet) {
                        Ok(()) => break,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if std::future::poll_fn(|cx| writable.as_mut().poll_writable(cx))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        },
                        Err(_) => return,
                    }
                }
            }
        });
        let a = tunnel::connect(
            relay,
            "relay.test",
            &roots,
            Request::Assist {
                token,
                ipk: ipk_a,
                peer: ipk_b,
                sign: Arc::new(move |message| Ok(key_a.sign(message).to_bytes())),
            },
        )
        .await
        .unwrap();
        let b = tunnel::connect(
            relay,
            "relay.test",
            &roots,
            Request::Assist {
                token,
                ipk: ipk_b,
                peer: ipk_a,
                sign: Arc::new(move |message| Ok(key_b.sign(message).to_bytes())),
            },
        )
        .await
        .unwrap();

        let (ep_a, _, routes_a, socket_a) = peer_endpoint_with_socket();
        let (ep_b, _, routes_b) = peer_endpoint();
        let synth_a = routes_a.lock().register(token, relay);
        let synth_b = routes_b.lock().register(token, relay);
        assert!(routes_a.lock().install_tcp(synth_a, a.clone()));
        assert!(routes_b.lock().install_tcp(synth_b, b.clone()));
        let run = tokio::time::timeout(Duration::from_secs(8), async {
            let (conn_a, conn_b) = tokio::join!(
                async { ep_a.connect(synth_a, "peer").unwrap().await.unwrap() },
                async { ep_b.accept().await.unwrap().await.unwrap() },
            );
            roundtrip(&conn_a, &conn_b, b"end-to-end bytes over authenticated TCP").await;
            assert!(routes_a.lock().tcp_active(synth_a));
            assert!(routes_b.lock().tcp_active(synth_b));
            assert!(!routes_a.lock().is_direct(synth_a));
            assert_eq!(conn_a.remote_address(), synth_a);

            // This current-thread test does not yield while filling the
            // queue, so its writer cannot drain behind the assertion. The
            // shared socket sheds the overflow and still serves another UDP
            // destination, rather than attaching its poller to this queue.
            let unrelated = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap();
            for _ in 0..=tunnel::QUEUE_PACKETS {
                if a.try_send(b"\x00").is_err() {
                    break;
                }
            }
            assert_eq!(a.try_send(b"\x00").unwrap_err().kind(), io::ErrorKind::WouldBlock);
            let drops = super::super::diagnostics::snapshot().tcp_queue_drops;
            socket_a
                .try_send(&udp::Transmit {
                    destination: synth_a,
                    ecn: None,
                    contents: b"\x00",
                    segment_size: None,
                    src_ip: None,
                })
                .unwrap();
            assert!(super::super::diagnostics::snapshot().tcp_queue_drops > drops);
            socket_a
                .try_send(&udp::Transmit {
                    destination: unrelated.local_addr().unwrap(),
                    ecn: None,
                    contents: b"other peer",
                    segment_size: None,
                    src_ip: None,
                })
                .unwrap();
            let mut other = [0u8; 32];
            let (len, _) = unrelated.recv_from(&mut other).await.unwrap();
            assert_eq!(&other[..len], b"other peer");

            // Shared leases keep the pipe alive; final teardown closes it.
            assert_eq!(routes_a.lock().register(token, relay), synth_a);
            routes_a.lock().unregister(&token);
            assert!(!a.is_closed());
            roundtrip(&conn_a, &conn_b, b"one route lease remains").await;
            routes_a.lock().unregister(&token);
            a.closed().await;
            assert!(a.is_closed());
            ep_a.close(0u32.into(), b"test complete");
            ep_b.close(0u32.into(), b"test complete");
        })
        .await;
        relay_task.abort();
        blackhole_task.abort();
        run.expect("TCP-only peer QUIC timed out");
        assert!(
            udp_seen.load(Ordering::Relaxed) > 0,
            "legacy UDP setup was attempted and discarded"
        );
    }

    /// A full QUIC handshake + bidirectional stream complete end-to-end over
    /// a TURN bridge: two peer endpoints on loopback whose only path to each
    /// other is a stub relay forwarding `TurnData` by token. This is the
    /// exact mechanism the on-device relay-first path uses — synthetic
    /// address, wrap on send, forward at the relay, unwrap on receive.
    #[tokio::test]
    async fn quic_completes_over_turn_bridge() {
        // rustls needs its process-level provider (the app does this at init).
        let _ = common::quic::config::setup_crypto_provider();

        let (relay_addr, relay_task) = stub_relay().await;
        let (ep_a, pokes_a, turn_a) = peer_endpoint();
        let (ep_b, pokes_b, turn_b) = peer_endpoint();

        // Both register the shared token → their synthetic address, and alloc
        // at the relay so it learns both ends before the handshake starts.
        let token = [42u8; 16];
        let synth_a = turn_a.lock().register(token, relay_addr);
        let _synth_b = turn_b.lock().register(token, relay_addr);
        let alloc = RelayMsg::TurnAlloc { token }.encode();
        pokes_a.send(relay_addr, &alloc).await.unwrap();
        pokes_b.send(relay_addr, &alloc).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Acceptor accepts; dialer connects to its synthetic peer address.
        // Their only path is the relay bridge.
        let accept = tokio::spawn(async move {
            let inc = ep_b.accept().await.expect("inbound connection");
            inc.accept().unwrap().await.expect("accept-side handshake")
        });
        let run = tokio::time::timeout(Duration::from_secs(15), async move {
            let conn_a = ep_a.connect(synth_a, "peer").unwrap().await.expect("dial handshake");
            let conn_b = accept.await.unwrap();
            roundtrip(&conn_a, &conn_b, b"ping").await;
        })
        .await;

        relay_task.abort();
        run.expect("TURN bridge handshake + stream timed out");
    }

    /// The upgrade: a connection formed over the TURN bridge keeps working —
    /// same connection, same synthetic address — after both ends flip their
    /// egress to the peer's real address and the relay disappears. This is
    /// what the background punch does on device, minus the punch itself.
    #[tokio::test]
    async fn quic_upgrades_to_direct_mid_connection() {
        let _ = common::quic::config::setup_crypto_provider();

        let (relay_addr, relay_task) = stub_relay().await;
        let (ep_a, pokes_a, turn_a) = peer_endpoint();
        let (ep_b, pokes_b, turn_b) = peer_endpoint();
        let a_real = ep_a.local_addr().unwrap();
        let b_real = ep_b.local_addr().unwrap();

        let token = [43u8; 16];
        let synth_a = turn_a.lock().register(token, relay_addr);
        let _synth_b = turn_b.lock().register(token, relay_addr);
        let alloc = RelayMsg::TurnAlloc { token }.encode();
        pokes_a.send(relay_addr, &alloc).await.unwrap();
        pokes_b.send(relay_addr, &alloc).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let accept = tokio::spawn(async move {
            let inc = ep_b.accept().await.expect("inbound connection");
            inc.accept().unwrap().await.expect("accept-side handshake")
        });
        let run = tokio::time::timeout(Duration::from_secs(15), async move {
            let conn_a = ep_a.connect(synth_a, "peer").unwrap().await.expect("dial handshake");
            let conn_b = accept.await.unwrap();
            roundtrip(&conn_a, &conn_b, b"over-relay").await;

            // Both ends learn the peer's real address (what a validated
            // punch reports) and flip. Kill the relay: if anything still
            // depended on it, the second roundtrip would hang.
            assert!(turn_a.lock().set_direct(&token, b_real));
            assert!(turn_b.lock().set_direct(&token, a_real));
            relay_task.abort();
            roundtrip(&conn_a, &conn_b, b"over-direct").await;

            // quinn never saw the path change.
            assert_eq!(conn_a.remote_address(), synth_a);
        })
        .await;

        run.expect("direct upgrade broke the connection");
    }

    /// A network change binds a new UDP endpoint and uses a fresh bridge
    /// token. One relay is sufficient even while the old two-address bridge
    /// remains occupied, and old route cleanup cannot remove the new route.
    #[tokio::test]
    async fn fresh_socket_and_token_reconnect_through_one_occupied_relay() {
        let _ = common::quic::config::setup_crypto_provider();
        let (relay, relay_task) = stub_relay().await;
        let (old_a, old_pokes, old_routes) = peer_endpoint();
        let (b, b_pokes, b_routes) = peer_endpoint();
        let old_token = [45; 16];
        let old_synth = old_routes.lock().register(old_token, relay);
        b_routes.lock().register(old_token, relay);
        old_pokes.send(relay, &RelayMsg::TurnAlloc { token: old_token }.encode()).await.unwrap();
        b_pokes.send(relay, &RelayMsg::TurnAlloc { token: old_token }.encode()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            let (old_conn, old_remote) = tokio::join!(
                async { old_a.connect(old_synth, "peer").unwrap().await.unwrap() },
                async { b.accept().await.unwrap().await.unwrap() },
            );
            roundtrip(&old_conn, &old_remote, b"before network change").await;
            old_a.close(0u32.into(), b"network changed");
            old_remote.close(0u32.into(), b"reconnect");

            let (new_a, new_pokes, new_routes) = peer_endpoint();
            assert_ne!(new_a.local_addr().unwrap(), old_a.local_addr().unwrap());
            let fresh_token = [46; 16];
            let new_synth = new_routes.lock().register(fresh_token, relay);
            let new_remote_synth = b_routes.lock().register(fresh_token, relay);
            new_pokes
                .send(relay, &RelayMsg::TurnAlloc { token: fresh_token }.encode())
                .await
                .unwrap();
            b_pokes
                .send(relay, &RelayMsg::TurnAlloc { token: fresh_token }.encode())
                .await
                .unwrap();
            b_routes.lock().unregister(&old_token);
            assert_eq!(b_routes.lock().synth_for(&fresh_token), Some(new_remote_synth));
            let (new_conn, new_remote) = tokio::join!(
                async { new_a.connect(new_synth, "peer").unwrap().await.unwrap() },
                async { b.accept().await.unwrap().await.unwrap() },
            );
            roundtrip(&new_conn, &new_remote, b"after network change").await;
        })
        .await;
        relay_task.abort();
        result.expect("new network failed to establish fresh relay route");
    }

    /// Registering a heard-from source repairs inbound only: the route keeps
    /// egressing through the relay, so a peer that upgraded before us can
    /// reach us without us having validated a path back.
    #[test]
    fn accepting_a_peer_source_leaves_egress_relayed() {
        let mut r = TurnRoutes::default();
        let relay: SocketAddr = "203.0.113.9:40432".parse().unwrap();
        let peer: SocketAddr = "198.51.100.7:51820".parse().unwrap();
        let synth = r.register([7; 16], relay);

        assert!(r.accept_from(&[7; 16], peer));
        assert_eq!(r.synth_for_real(&peer), Some(synth));
        assert_eq!(r.egress(synth), Some(Egress::Relay { relay, token: [7; 16] }));

        // A route that has already gone away takes no registration with it.
        r.unregister(&[7; 16]);
        assert!(!r.accept_from(&[7; 16], peer));
        assert_eq!(r.synth_for_real(&peer), None);
    }

    /// The punch is one-sided far more often than not: the peer's pings reach
    /// us, ours die in their NAT. They validate that path and flip their
    /// egress to direct while we are still bridged, so every packet they send
    /// arrives raw from an address quinn was never told about — mid-handshake,
    /// where a client may not migrate, it simply drops them and the dial times
    /// out on a bridge that looked fine. Accepting the source we heard them
    /// from is what keeps that connection alive.
    #[tokio::test]
    async fn quic_completes_when_only_the_acceptor_upgrades() {
        let _ = common::quic::config::setup_crypto_provider();

        let (relay_addr, relay_task) = stub_relay().await;
        let (ep_a, pokes_a, turn_a) = peer_endpoint();
        let (ep_b, pokes_b, turn_b) = peer_endpoint();
        let a_real = ep_a.local_addr().unwrap();
        let b_real = ep_b.local_addr().unwrap();

        let token = [44u8; 16];
        let synth_a = turn_a.lock().register(token, relay_addr);
        let _synth_b = turn_b.lock().register(token, relay_addr);
        let alloc = RelayMsg::TurnAlloc { token }.encode();
        pokes_a.send(relay_addr, &alloc).await.unwrap();
        pokes_b.send(relay_addr, &alloc).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The acceptor's punch validated a path first, so it is already
        // egressing direct when the dial starts. The dialer never validates
        // one and stays on the relay — it only knows the address it heard the
        // peer's pings from.
        assert!(turn_b.lock().set_direct(&token, a_real));
        assert!(turn_a.lock().accept_from(&token, b_real));

        let accept = tokio::spawn(async move {
            let inc = ep_b.accept().await.expect("inbound connection");
            inc.accept().unwrap().await.expect("accept-side handshake")
        });
        let run = tokio::time::timeout(Duration::from_secs(15), async move {
            let conn_a = ep_a.connect(synth_a, "peer").unwrap().await.expect("dial handshake");
            let conn_b = accept.await.unwrap();
            roundtrip(&conn_a, &conn_b, b"one-sided").await;
            assert_eq!(conn_a.remote_address(), synth_a);
        })
        .await;

        relay_task.abort();
        run.expect("a one-sided upgrade stranded the connection");
    }
}
