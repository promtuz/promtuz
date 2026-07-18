//! The P2P socket: one UDP port that carries both QUIC and our disco
//! pokes, so the NAT hole a poke opens is the one the QUIC handshake
//! reuses.
//!
//! On receive it splits the two: a datagram that looks like disco
//! ([`disco::peek_channel`]) is handed to the punch layer over `inbox`
//! and never reaches quinn; everything else is a QUIC packet. Pokes go
//! out through [`PokeSender`] on the same socket. quinn drives QUIC
//! through the [`quinn::AsyncUdpSocket`] trait as usual, and
//! `grease_quic_bit(false)` (set in [`build_endpoint`]) makes it drop any
//! stray poke that reaches it.
//!
//! ponytail: naive one-datagram-per-recv, no GSO/GRO — fine for the
//! pokes and the handshake. If bulk device-to-device transfer throughput
//! needs it, back this with `quinn::udp::UdpSocketState` and split GRO
//! batches by stride before the disco/QUIC demux.

use std::io;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use anyhow::Result;
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

/// An inbound disco poke: the sender's address and the raw sealed bytes.
pub type Poke = (SocketAddr, Vec<u8>);

/// Sends disco pokes on the P2P socket — the same port quinn uses, so
/// pokes and the QUIC handshake share one NAT mapping.
#[derive(Clone)]
pub struct PokeSender {
    io: Arc<UdpSocket>,
}

impl PokeSender {
    pub async fn send(&self, to: SocketAddr, bytes: &[u8]) -> io::Result<()> {
        self.io.send_to(bytes, to).await.map(|_| ())
    }
}

/// The custom socket handed to quinn. Peels disco off the QUIC stream.
#[derive(Debug)]
pub struct PunchSocket {
    io: Arc<UdpSocket>,
    inbox_tx: mpsc::UnboundedSender<Poke>,
}

/// What one bound P2P socket yields: the socket for quinn, a poke sender,
/// and the inbound-poke stream for the punch layer.
pub struct Bound {
    pub socket: Arc<PunchSocket>,
    pub pokes: PokeSender,
    pub inbox: mpsc::UnboundedReceiver<Poke>,
}

impl PunchSocket {
    /// Bind the P2P UDP socket. Must run inside the tokio runtime — it
    /// registers with the reactor.
    pub fn bind(addr: SocketAddr) -> io::Result<Bound> {
        let std_sock = std::net::UdpSocket::bind(addr)?;
        std_sock.set_nonblocking(true)?;
        let io = Arc::new(UdpSocket::from_std(std_sock)?);
        let (inbox_tx, inbox) = mpsc::unbounded_channel();
        Ok(Bound {
            socket: Arc::new(Self { io: io.clone(), inbox_tx }),
            pokes: PokeSender { io },
            inbox,
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
        let r = self.io.try_send_to(transmit.contents, transmit.destination).map(|_| ());
        if let Err(e) = &r
            && e.kind() != io::ErrorKind::WouldBlock
        {
            // TEMP diagnostic: a failing QUIC send stalls the handshake.
            log::warn!("P2P sock: send {}B to {} failed: {e}", transmit.contents.len(), transmit.destination);
        }
        r
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Drain pokes; return on the first real QUIC datagram (or Pending).
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
            meta[0] = udp::RecvMeta { addr: src, len, stride: len, ecn: None, dst_ip: None };
            // TEMP diagnostic: shows whether the peer's QUIC Initial actually
            // reaches this socket (vs. only disco crossing).
            log::info!("P2P sock: <- quic {len}B from {src}");
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

/// Build the P2P endpoint on a fresh punch socket. Client and server
/// configs are both the self-signed peer identity — we dial some peers
/// and accept others on the one endpoint. `grease_quic_bit(false)` lets a
/// stray poke be dropped rather than mis-parsed as QUIC.
pub fn build_endpoint() -> Result<(Endpoint, PokeSender, mpsc::UnboundedReceiver<Poke>)> {
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
    Ok((endpoint, bound.pokes, bound.inbox))
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
                    }
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
}
