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
use std::task::Context;
use std::task::Poll;

use anyhow::Result;
use common::proto::p2p_relay::RelayMsg;
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

/// An inbound disco poke: the sender's address and the raw sealed bytes.
pub type Poke = (SocketAddr, Vec<u8>);

/// A relay's STUN echo: the query's tx-id and the public address it saw us
/// from.
pub type StunReply = ([u8; 8], SocketAddr);

/// Where a TURN-bridged session's quinn packets really go.
#[derive(Debug)]
struct Route {
    relay: SocketAddr,
    token: [u8; 16],
}

/// Maps between TURN bridge tokens and the synthetic peer addresses quinn
/// uses for them. Shared between the session manager (which registers a
/// bridge) and the socket (which wraps outbound and unwraps inbound TURN
/// datagrams). The synthetic address is a pure quinn-side handle — packets
/// to it are redirected to the relay, never sent to it.
#[derive(Debug, Default)]
pub struct TurnRoutes {
    by_synth: HashMap<SocketAddr, Route>,
    by_token: HashMap<[u8; 16], SocketAddr>,
    next:     u32,
}

impl TurnRoutes {
    /// Register (idempotently) a bridge to `relay` under `token`, returning
    /// the synthetic, unreachable peer address quinn should dial/accept for
    /// it.
    pub fn register(&mut self, token: [u8; 16], relay: SocketAddr) -> SocketAddr {
        if let Some(&synth) = self.by_token.get(&token) {
            return synth;
        }
        self.next += 1;
        let n = self.next;
        // A unique address in the RFC 6666 discard prefix (100::/64): never
        // routable, never a real candidate — a pure quinn-side handle.
        let synth = SocketAddr::new(
            Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, (n >> 16) as u16, n as u16).into(),
            9,
        );
        self.by_synth.insert(synth, Route { relay, token });
        self.by_token.insert(token, synth);
        synth
    }

    pub fn unregister(&mut self, token: &[u8; 16]) {
        if let Some(synth) = self.by_token.remove(token) {
            self.by_synth.remove(&synth);
        }
    }

    /// If `dest` is a synthetic TURN address, the `(relay, token)` its quinn
    /// packets must be wrapped and sent to.
    fn wrap_target(&self, dest: SocketAddr) -> Option<(SocketAddr, [u8; 16])> {
        self.by_synth.get(&dest).map(|r| (r.relay, r.token))
    }

    /// The synthetic address for an inbound TURN datagram's token, if we
    /// have a session for it.
    fn synth_for(&self, token: &[u8; 16]) -> Option<SocketAddr> {
        self.by_token.get(token).copied()
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
    io:       Arc<UdpSocket>,
    inbox_tx: mpsc::UnboundedSender<Poke>,
    stun_tx:  mpsc::UnboundedSender<StunReply>,
    turn:     Arc<Mutex<TurnRoutes>>,
}

/// What one bound P2P socket yields: the socket for quinn, a poke sender,
/// the inbound-poke stream, the relay STUN-echo stream, and the shared TURN
/// routing table.
pub struct Bound {
    pub socket:  Arc<PunchSocket>,
    pub pokes:   PokeSender,
    pub inbox:   mpsc::UnboundedReceiver<Poke>,
    pub stun_rx: mpsc::UnboundedReceiver<StunReply>,
    pub turn:    Arc<Mutex<TurnRoutes>>,
}

impl PunchSocket {
    /// Bind the P2P UDP socket. Must run inside the tokio runtime — it
    /// registers with the reactor.
    pub fn bind(addr: SocketAddr) -> io::Result<Bound> {
        let std_sock = std::net::UdpSocket::bind(addr)?;
        std_sock.set_nonblocking(true)?;
        let io = Arc::new(UdpSocket::from_std(std_sock)?);
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        let (stun_tx, stun_rx) = mpsc::unbounded_channel();
        let turn = Arc::new(Mutex::new(TurnRoutes::default()));
        Ok(Bound {
            socket: Arc::new(Self { io: io.clone(), inbox_tx, stun_tx, turn: turn.clone() }),
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
        let wrap = self.turn.lock().wrap_target(transmit.destination);
        match wrap {
            // TURN path: wrap the QUIC datagram so the relay forwards it to
            // the peer under this bridge's token.
            Some((relay, token)) => {
                let framed = RelayMsg::TurnData { token, payload: transmit.contents }.encode();
                log::trace!("P2P: TURN send {}B -> {}", transmit.contents.len(), addr_short(relay));
                self.io.try_send_to(&framed, relay).map(|_| ())
            },
            None => self.io.try_send_to(transmit.contents, transmit.destination).map(|_| ()),
        }
    }

    fn poll_recv(
        &self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Drain disco + TURN; return on the first real QUIC datagram (or
        // Pending).
        loop {
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
                let _ = self.inbox_tx.send((src, bufs[0][..len].to_vec()));
                continue;
            }
            // Relay-assist? Only TURN data is a QUIC datagram bound for
            // quinn. Extract just Copy data so bufs[0]'s borrow ends before
            // we may rewrite it in place.
            let turn = match RelayMsg::decode(&bufs[0][..len]) {
                Some(RelayMsg::TurnData { token, payload }) => Some((token, payload.len())),
                Some(RelayMsg::StunResp { tx, seen }) => {
                    let _ = self.stun_tx.send((tx, seen));
                    continue;
                },
                Some(_) => continue, // StunReq/TurnAlloc — never sent to a client
                None => None,
            };
            if let Some((token, plen)) = turn {
                let Some(synth) = self.turn.lock().synth_for(&token) else { continue };
                // TEMP diagnostic: TURN recv path (strip once handshake works).
                log::info!("P2P: TURN recv {plen}B <- relay, as {synth}");
                // Present the bridged QUIC payload to quinn as if it came
                // direct from the peer's synthetic address.
                let off = len - plen;
                bufs[0].copy_within(off..len, 0);
                meta[0] =
                    udp::RecvMeta { addr: synth, len: plen, stride: plen, ecn: None, dst_ip: None };
                return Poll::Ready(Ok(1));
            }
            meta[0] = udp::RecvMeta { addr: src, len, stride: len, ecn: None, dst_ip: None };
            return Poll::Ready(Ok(1));
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
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
    pub pokes:    PokeSender,
    pub inbox:    mpsc::UnboundedReceiver<Poke>,
    pub stun_rx:  mpsc::UnboundedReceiver<StunReply>,
    pub turn:     Arc<Mutex<TurnRoutes>>,
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

    let mut endpoint = Endpoint::new_with_abstract_socket(
        ep_cfg,
        Some(build_peer_server_cfg(&identity)?),
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;

    fn empty_meta() -> udp::RecvMeta {
        udp::RecvMeta {
            addr:   (Ipv4Addr::UNSPECIFIED, 0).into(),
            len:    0,
            stride: 0,
            ecn:    None,
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
        assert_eq!(r.wrap_target(s1), Some((relay, [1; 16])));
        assert_eq!(r.synth_for(&[1; 16]), Some(s1));
        // a real (non-synthetic) address passes straight through
        assert_eq!(r.wrap_target("1.2.3.4:5".parse().unwrap()), None);
        r.unregister(&[1; 16]);
        assert_eq!(r.wrap_target(s1), None);
        assert_eq!(r.synth_for(&[1; 16]), None);
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
        let poke = disco::DiscoKey::new(&[3u8; 32], [4; 8])
            .seal(&disco::DiscoMsg::Ping { tx: [1; 8] });
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

    /// A full QUIC handshake + bidirectional stream complete end-to-end over
    /// a TURN bridge: two peer endpoints on loopback whose only path to each
    /// other is a stub relay forwarding `TurnData` by token. This is the
    /// exact fallback mechanism the on-device path uses — synthetic address,
    /// wrap on send, forward at the relay, unwrap on receive — and the one
    /// leg the two-phone tests never got to finish (they died on token
    /// pairing before the handshake could complete).
    #[tokio::test]
    async fn quic_completes_over_turn_bridge() {
        use std::collections::HashMap;
        use std::net::Ipv6Addr;

        use ed25519_dalek::SigningKey;

        use crate::quic::peer_config::test_peer_configs;

        // rustls needs its process-level provider (the app does this at init).
        let _ = common::quic::config::setup_crypto_provider();

        // Stub relay: forward each TurnData to the other source seen under its
        // token — the minimal version of the real bridge.
        let relay = Arc::new(UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).await.unwrap());
        let relay_addr = relay.local_addr().unwrap();
        let relay_task = {
            let relay = relay.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1600];
                let mut ends: HashMap<[u8; 16], Vec<SocketAddr>> = HashMap::new();
                while let Ok((n, src)) = relay.recv_from(&mut buf).await {
                    let (token, is_data) = match RelayMsg::decode(&buf[..n]) {
                        Some(RelayMsg::TurnAlloc { token }) => (token, false),
                        Some(RelayMsg::TurnData { token, .. }) => (token, true),
                        _ => continue,
                    };
                    let list = ends.entry(token).or_default();
                    if !list.contains(&src) {
                        list.push(src);
                    }
                    if is_data
                        && let Some(&dst) = list.iter().find(|&&a| a != src)
                    {
                        let _ = relay.send_to(&buf[..n], dst).await;
                    }
                }
            })
        };

        // Two peer endpoints (same throwaway key — the verifier accepts any
        // valid self-signed Ed25519 cert), each on a fresh punch socket.
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let mk = || {
            let bound = PunchSocket::bind((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
            let (server_cfg, client_cfg) = test_peer_configs(&key).unwrap();
            let mut ep_cfg = EndpointConfig::default();
            ep_cfg.grease_quic_bit(false);
            let mut ep = Endpoint::new_with_abstract_socket(
                ep_cfg,
                Some(server_cfg),
                bound.socket,
                Arc::new(TokioRuntime),
            )
            .unwrap();
            ep.set_default_client_config(client_cfg);
            (ep, bound.pokes, bound.turn)
        };
        let (ep_a, pokes_a, turn_a) = mk();
        let (ep_b, pokes_b, turn_b) = mk();

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

            // Data flows both ways over the bridge.
            let (mut send, mut recv) = conn_a.open_bi().await.unwrap();
            send.write_all(b"ping").await.unwrap();
            send.finish().unwrap();
            let (mut bsend, mut brecv) = conn_b.accept_bi().await.unwrap();
            assert_eq!(brecv.read_to_end(16).await.unwrap(), b"ping");
            bsend.write_all(b"pong").await.unwrap();
            bsend.finish().unwrap();
            assert_eq!(recv.read_to_end(16).await.unwrap(), b"pong");
        })
        .await;

        relay_task.abort();
        run.expect("TURN bridge handshake + stream timed out");
    }
}
