//! STUN echo + blind TURN bridge, sharing the relay's existing QUIC UDP
//! socket — no extra port, no extra firewall rule.
//!
//! [`AssistSocket`] wraps the QUIC socket: it peels our `.pRr` datagrams
//! off the receive path and hands them to [`serve`], passing every real
//! QUIC packet straight through to quinn. So the one already-open UDP port
//! carries client messaging and hole-punch assist together.
//!
//! Two clients that can't hole-punch (symmetric NAT, or v6-only ↔ v4-only)
//! both already reach this relay, so it bridges them: each sends a
//! [`RelayMsg::TurnAlloc`] under a shared secret token, then their own QUIC
//! rides [`RelayMsg::TurnData`] datagrams the relay forwards verbatim to
//! the other endpoint under that token. The relay reads only the token;
//! peer QUIC stays end-to-end encrypted, and the relay never needs its own
//! public address — the client already holds it (that's the relay it
//! dialed).
//!
//! STUN is the free half: a client asks from its P2P socket and learns the
//! public address that socket maps to, so a cone-NAT peer can be punched
//! without paying for the bridge.
//!
//! ponytail: the wrapper is naive (no GSO/GRO batching) and one task owns
//! the bridge table (no lock; bounded by a 30s idle sweep + a hard cap).
//! Both are fine at small-relay scale; back the socket with
//! `quinn::udp::UdpSocketState` if QUIC throughput ever needs the batches.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use std::time::Instant;

use common::info;
use common::proto::p2p_relay::RelayMsg;
use common::proto::p2p_relay::TOKEN_LEN;
use common::proto::p2p_relay::is_assist;
use quinn::AsyncUdpSocket;
use quinn::UdpPoller;
use quinn::udp;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// One peeled assist datagram: source address + raw bytes.
type Assist = (SocketAddr, Vec<u8>);

/// The relay's QUIC socket, wrapped so hole-punch assist datagrams are
/// split off before quinn sees them.
#[derive(Debug)]
pub struct AssistSocket {
    io:    Arc<UdpSocket>,
    inbox: mpsc::UnboundedSender<Assist>,
}

/// What [`serve`] needs: the peeled-assist stream, and a handle to send
/// replies/forwards back out the same socket.
pub struct AssistInbox {
    rx:   mpsc::UnboundedReceiver<Assist>,
    sock: Arc<UdpSocket>,
}

impl std::fmt::Debug for AssistInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistInbox").finish_non_exhaustive()
    }
}

/// Wrap an already-bound std UDP socket for quinn, splitting off assist
/// datagrams. Must run inside the tokio runtime (registers with the
/// reactor).
pub fn wrap_socket(std_sock: std::net::UdpSocket) -> io::Result<(Arc<AssistSocket>, AssistInbox)> {
    std_sock.set_nonblocking(true)?;
    let io = Arc::new(UdpSocket::from_std(std_sock)?);
    let (inbox, rx) = mpsc::unbounded_channel();
    let sock = Arc::new(AssistSocket { io: io.clone(), inbox });
    Ok((sock, AssistInbox { rx, sock: io }))
}

impl AsyncUdpSocket for AssistSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(AssistPoller { io: self.io.clone() })
    }

    fn try_send(&self, transmit: &udp::Transmit) -> io::Result<()> {
        self.io.try_send_to(transmit.contents, transmit.destination).map(|_| ())
    }

    fn poll_recv(
        &self, cx: &mut Context, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // Peel assist datagrams to the handler; surface the first real QUIC
        // datagram to quinn (or Pending).
        loop {
            let (len, src) = {
                let mut rb = tokio::io::ReadBuf::new(&mut bufs[0]);
                match self.io.poll_recv_from(cx, &mut rb) {
                    Poll::Ready(Ok(src)) => (rb.filled().len(), src),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            };
            if is_assist(&bufs[0][..len]) {
                let _ = self.inbox.send((src, bufs[0][..len].to_vec()));
                continue;
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
struct AssistPoller {
    io: Arc<UdpSocket>,
}

impl UdpPoller for AssistPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.io.poll_send_ready(cx)
    }
}

// ---- the bridge ----

/// Drop a bridge whose endpoints have both been silent this long.
const IDLE_TTL: Duration = Duration::from_secs(60);
/// How often to sweep idle bridges.
const SWEEP: Duration = Duration::from_secs(30);
/// Cap concurrent bridges so junk allocations can't grow the map without
/// bound — well above any real concurrent-call count.
const MAX_BRIDGES: usize = 4096;

/// The two ends of one bridge, learned from their datagrams' source
/// addresses.
struct Bridge {
    a:    SocketAddr,
    b:    Option<SocketAddr>,
    seen: Instant,
}

impl Bridge {
    /// The far side of `src` — registering `src` as the second end if
    /// there's a free slot. `None` if `src` is a third source on this token.
    fn other(&mut self, src: SocketAddr, now: Instant) -> Option<SocketAddr> {
        self.seen = now;
        if src == self.a {
            self.b
        } else if Some(src) == self.b {
            Some(self.a)
        } else if self.b.is_none() {
            self.b = Some(src);
            Some(self.a)
        } else {
            None
        }
    }
}

pub async fn serve(mut assist: AssistInbox, cancel: CancellationToken) {
    info!("relay assist (STUN/TURN) sharing the QUIC port");
    let mut bridges: HashMap<[u8; TOKEN_LEN], Bridge> = HashMap::new();
    let mut sweep = tokio::time::interval(SWEEP);

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = sweep.tick() => {
                let now = Instant::now();
                bridges.retain(|_, br| now.duration_since(br.seen) < IDLE_TTL);
            },
            got = assist.rx.recv() => {
                let Some((src, pkt)) = got else { break };
                handle(&assist.sock, &pkt, src, &mut bridges).await;
            },
        }
    }
}

async fn handle(
    sock: &UdpSocket, pkt: &[u8], src: SocketAddr, bridges: &mut HashMap<[u8; TOKEN_LEN], Bridge>,
) {
    match RelayMsg::decode(pkt) {
        Some(RelayMsg::StunReq { tx }) => {
            let _ = sock.send_to(&RelayMsg::StunResp { tx, seen: src }.encode(), src).await;
        },
        Some(RelayMsg::TurnAlloc { token }) => {
            let now = Instant::now();
            if let Some(br) = get_or_insert(bridges, token, src, now) {
                br.other(src, now); // register the source; no data to forward
                // TEMP diagnostic: which source registers under which token.
                info!("TURN alloc {} tok {:02x?} (a={}, b={:?})", src, &token[..4], br.a, br.b);
            }
        },
        Some(RelayMsg::TurnData { token, payload }) => {
            let now = Instant::now();
            let dst = get_or_insert(bridges, token, src, now).and_then(|br| br.other(src, now));
            match dst {
                // Forward verbatim — the receiver parses the token and hands
                // the QUIC payload to its own stack.
                Some(dst) => {
                    // TEMP diagnostic: strip once TURN handshake works.
                    info!("TURN fwd {}B {} -> {} tok {:02x?}", payload.len(), src, dst, &token[..4]);
                    let _ = sock.send_to(pkt, dst).await;
                },
                None => {
                    info!("TURN drop {}B from {} tok {:02x?} (no peer)", payload.len(), src, &token[..4])
                },
            }
        },
        // StunResp is a reply, never inbound here; junk decodes to None.
        Some(RelayMsg::StunResp { .. }) | None => {},
    }
}

/// Fetch `token`'s bridge, creating it (with `src` as the first end) if
/// absent and there's room. Sweeps idle entries before rejecting on a full
/// map so a burst doesn't wedge it.
fn get_or_insert(
    bridges: &mut HashMap<[u8; TOKEN_LEN], Bridge>, token: [u8; TOKEN_LEN], src: SocketAddr,
    now: Instant,
) -> Option<&mut Bridge> {
    if !bridges.contains_key(&token) {
        if bridges.len() >= MAX_BRIDGES {
            bridges.retain(|_, br| now.duration_since(br.seen) < IDLE_TTL);
            if bridges.len() >= MAX_BRIDGES {
                return None;
            }
        }
        bridges.insert(token, Bridge { a: src, b: None, seen: now });
    }
    bridges.get_mut(&token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_to_the_other_end() {
        let mut bridges = HashMap::new();
        let a: SocketAddr = "1.1.1.1:1".parse().unwrap();
        let b: SocketAddr = "2.2.2.2:2".parse().unwrap();
        let now = Instant::now();

        // a allocs, then b's data forwards to a.
        get_or_insert(&mut bridges, [9; TOKEN_LEN], a, now).unwrap().other(a, now);
        let dst = get_or_insert(&mut bridges, [9; TOKEN_LEN], b, now).and_then(|br| br.other(b, now));
        assert_eq!(dst, Some(a));
        // a's data now forwards to b.
        let dst = get_or_insert(&mut bridges, [9; TOKEN_LEN], a, now).and_then(|br| br.other(a, now));
        assert_eq!(dst, Some(b));
        // a third source on the same token is ignored.
        let c: SocketAddr = "3.3.3.3:3".parse().unwrap();
        let dst = get_or_insert(&mut bridges, [9; TOKEN_LEN], c, now).and_then(|br| br.other(c, now));
        assert_eq!(dst, None);
    }

    #[test]
    fn cap_rejects_when_full_and_all_fresh() {
        let mut bridges = HashMap::new();
        let now = Instant::now();
        let src: SocketAddr = "1.1.1.1:1".parse().unwrap();
        for i in 0..MAX_BRIDGES {
            let mut token = [0u8; TOKEN_LEN];
            token[..8].copy_from_slice(&(i as u64).to_le_bytes());
            assert!(get_or_insert(&mut bridges, token, src, now).is_some());
        }
        assert!(get_or_insert(&mut bridges, [0xff; TOKEN_LEN], src, now).is_none());
    }
}
