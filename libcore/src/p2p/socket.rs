//! The P2P socket: one UDP port carries QUIC, disco pokes and relay-assist datagrams, so the NAT
//! hole a poke opens is the one the QUIC handshake reuses.

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
use common::quic::config::build_self_signed_ed25519_cert;
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
use crate::data::identity::IdentitySigner;
use crate::quic::peer_config::build_peer_client_cfg;
use crate::quic::peer_config::build_peer_server_cfg;
use crate::utils::addr_short;

/// Anything past this many pending handshakes is unauthenticated UDP piling up.
const MAX_INCOMING: usize = 16;

pub type Poke = (SocketAddr, Vec<u8>);

/// The relay, the query's tx id, and the address the relay saw us from.
pub type StunReply = (SocketAddr, [u8; 8], SocketAddr);

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
    worker: Option<tokio::task::AbortHandle>,
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

/// Maps TURN bridge tokens to synthetic peer addresses. quinn only sees the synthetic address,
/// which stays fixed for the connection's life whichever real path carries the traffic.
#[derive(Debug, Default)]
pub struct TurnRoutes {
    by_synth: HashMap<SocketAddr, Egress>,
    // Refcounted: a delayed offer can register a live bridge again, and its timeout must not
    // remove the first connection's route.
    by_token: HashMap<[u8; 16], (SocketAddr, usize)>,
    /// Real peer address to synth, for relabeling inbound direct datagrams.
    by_real: HashMap<SocketAddr, SocketAddr>,
    relays: HashMap<[u8; 16], SocketAddr>,
    tcp: HashMap<SocketAddr, TcpRoute>,
    recv_waker: Option<Waker>,
    recv_cursor: usize,
    next: u32,
}

impl TurnRoutes {
    /// Every call must be paired with one `unregister`.
    pub fn register(&mut self, token: [u8; 16], relay: SocketAddr) -> SocketAddr {
        if let Some((synth, owners)) = self.by_token.get_mut(&token) {
            *owners += 1;
            return *synth;
        }
        self.next += 1;
        let n = self.next;
        // Unique in the RFC 6666 discard prefix (100::/64), so it is never routable.
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

    /// Flips egress to raw UDP toward `addr`. Relay ingress stays live, so nothing in flight is
    /// dropped. `false` when the route is already gone.
    pub fn set_direct(&mut self, token: &[u8; 16], addr: SocketAddr) -> bool {
        let Some(&(synth, _)) = self.by_token.get(token) else { return false };
        self.by_synth.insert(synth, Egress::Direct { addr });
        self.by_real.insert(super::reflexive::canonical(addr), synth);
        true
    }

    /// Accepts inbound datagrams from `src` without touching our egress, for a one-sided punch
    /// where the peer went direct and we still send relayed. `false` when the route is gone.
    pub fn accept_from(&mut self, token: &[u8; 16], src: SocketAddr) -> bool {
        let Some(&(synth, _)) = self.by_token.get(token) else { return false };
        self.by_real.insert(super::reflexive::canonical(src), synth);
        true
    }

    fn egress(&self, dest: SocketAddr) -> Option<Egress> {
        match self.by_synth.get(&dest).copied() {
            Some(Egress::Relay { .. }) if self.tcp.get(&dest).is_some_and(|r| r.active) => {
                Some(Egress::Tcp)
            },
            route => route,
        }
    }

    fn synth_for_real(&self, src: &SocketAddr) -> Option<SocketAddr> {
        self.by_real.get(&super::reflexive::canonical(*src)).copied()
    }

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

    pub(super) fn own_tcp_worker(&mut self, synth: SocketAddr, worker: tokio::task::AbortHandle) {
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

#[derive(Clone)]
pub struct PokeSender {
    io: Arc<UdpSocket>,
}

impl PokeSender {
    pub async fn send(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        self.io.send_to(bytes, to).await.map(|_| ())
    }
}

#[derive(Debug)]
pub struct PunchSocket {
    io: Arc<UdpSocket>,
    inbox_tx: mpsc::Sender<Poke>,
    stun_tx: mpsc::Sender<StunReply>,
    turn: Arc<Mutex<TurnRoutes>>,
    tcp_first: AtomicBool,
}

pub struct Bound {
    pub socket: Arc<PunchSocket>,
    pub pokes: PokeSender,
    pub inbox: mpsc::Receiver<Poke>,
    pub stun_rx: mpsc::Receiver<StunReply>,
    pub turn: Arc<Mutex<TurnRoutes>>,
}

impl PunchSocket {
    /// Must run inside the tokio runtime: it registers with the reactor.
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
        // quinn's max_transmit_segments stays 1, so `contents` is always a single datagram.
        let (egress, tcp) = {
            let routes = self.turn.lock();
            (routes.egress(transmit.destination), routes.tcp_channel(transmit.destination))
        };
        let result = match egress {
            Some(Egress::Relay { relay, token }) => {
                let framed = RelayMsg::TurnData { token, payload: transmit.contents }.encode();
                log::trace!("P2P: TURN send {}B -> {}", transmit.contents.len(), addr_short(relay));
                let udp = self.io.try_send_to(&framed, relay).map(|_| ());
                if udp.is_ok() {
                    super::diagnostics::sent_datagram(true, transmit.contents.len());
                }
                // Keep UDP egress until TCP ingress proves both peers joined: the authenticated
                // bridge cannot join a legacy UDP one, which an old peer may still be on.
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
                        // quinn polls writability without a destination, so waiting on one TCP
                        // queue would stall other peers. Shed it like UDP loss; QUIC retransmits.
                        super::diagnostics::dropped_tcp_datagram();
                        return Ok(());
                    },
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                        self.turn.lock().remove_tcp(transmit.destination, &channel);
                        // The failure is this route's, not the shared endpoint's; QUIC retries.
                        return Ok(());
                    },
                    result => result,
                },
                None => Err(io::Error::new(io::ErrorKind::NotConnected, "TCP route retired")),
            },
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
        // Alternate sources so neither bulk TCP nor UDP starves the other. Installing a channel
        // wakes this poll.
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
                let _ = self.inbox_tx.try_send((src, bufs[0][..len].to_vec()));
                continue;
            }
            let turn = match RelayMsg::decode(&bufs[0][..len]) {
                Some(RelayMsg::TurnData { token, payload }) => Some((token, payload.len())),
                Some(RelayMsg::StunResp { tx, seen }) => {
                    let _ = self.stun_tx.try_send((src, tx, seen));
                    continue;
                },
                Some(_) => continue, // StunReq/TurnAlloc: never sent to a client
                None => None,
            };
            if let Some((token, plen)) = turn {
                let Some(synth) = self.turn.lock().synth_for_relay(&token, src) else { continue };
                let off = len - plen;
                bufs[0].copy_within(off..len, 0);
                meta[0] =
                    udp::RecvMeta { addr: synth, len: plen, stride: plen, ecn: None, dst_ip: None };
                return Poll::Ready(Ok(1));
            }
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

pub struct BuiltEndpoint {
    pub endpoint: Endpoint,
    pub pokes: PokeSender,
    pub inbox: mpsc::Receiver<Poke>,
    pub stun_rx: mpsc::Receiver<StunReply>,
    pub turn: Arc<Mutex<TurnRoutes>>,
}

/// `grease_quic_bit(false)` makes quinn drop a stray poke instead of parsing it as QUIC.
pub fn build_endpoint() -> Result<BuiltEndpoint> {
    let bound = PunchSocket::bind((Ipv6Addr::UNSPECIFIED, 0).into())?;
    let key = Arc::new(build_self_signed_ed25519_cert(IdentitySigner::tls_subkey()?));

    let mut ep_cfg = EndpointConfig::default();
    ep_cfg.grease_quic_bit(false);

    let mut server_cfg = build_peer_server_cfg(key.clone())?;
    server_cfg.max_incoming(MAX_INCOMING);

    let mut endpoint = Endpoint::new_with_abstract_socket(
        ep_cfg,
        Some(server_cfg),
        bound.socket,
        Arc::new(TokioRuntime),
    )?;
    endpoint.set_default_client_config(build_peer_client_cfg(key)?);
    Ok(BuiltEndpoint {
        endpoint,
        pokes: bound.pokes,
        inbox: bound.inbox,
        stun_rx: bound.stun_rx,
        turn: bound.turn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_routes_count_owners_and_admit_relay_ingress_only_from_the_registered_relay() {
        let mut r = TurnRoutes::default();
        let relay: SocketAddr = "203.0.113.1:443".parse().unwrap();
        let other_relay: SocketAddr = "203.0.113.2:443".parse().unwrap();
        let (a, b) = ([1; 16], [2; 16]);
        let synth = r.register(a, relay);
        assert_ne!(r.register(b, relay), synth, "one synthetic address per bridge");
        assert_eq!(r.register(a, relay), synth, "a second owner shares the route");
        assert_eq!(r.egress(synth), Some(Egress::Relay { relay, token: a }));
        assert_eq!(r.egress("1.2.3.4:5".parse().unwrap()), None, "real addresses pass through");
        assert_eq!(r.synth_for_relay(&a, other_relay), None);
        assert_eq!(r.synth_for_relay(&a, "[::ffff:203.0.113.1]:443".parse().unwrap()), Some(synth));

        let heard: SocketAddr = "198.51.100.7:51820".parse().unwrap();
        assert!(r.accept_from(&a, heard));
        assert_eq!(r.synth_for_real(&heard), Some(synth), "a heard source is accepted inbound");
        assert_eq!(
            r.egress(synth),
            Some(Egress::Relay { relay, token: a }),
            "while we still send relayed"
        );

        let direct: SocketAddr = "198.51.100.9:5080".parse().unwrap();
        assert!(r.set_direct(&a, direct));
        assert_eq!(r.egress(synth), Some(Egress::Direct { addr: direct }));
        assert_eq!(r.synth_for_real(&direct), Some(synth));
        assert_eq!(r.synth_for_relay(&a, relay), Some(synth), "relay ingress stays live");
        assert_eq!(r.synth_for_relay(&a, other_relay), None, "and still only from its relay");

        r.unregister(&a);
        assert_eq!(r.synth_for(&a), Some(synth), "the other owner keeps the route");
        r.unregister(&a);
        assert_eq!(
            (r.egress(synth), r.synth_for_real(&heard), r.synth_for_real(&direct)),
            (None, None, None)
        );
        assert!(
            !r.set_direct(&a, direct) && !r.accept_from(&a, heard),
            "a gone route takes nothing"
        );
        assert_eq!(r.egress(r.synth_for(&b).unwrap()), Some(Egress::Relay { relay, token: b }));
    }
}
