//! STUN echo and a blind TURN bridge on the QUIC UDP socket. Off by default: a bridge token is an
//! unissued bearer secret, so whoever guesses one has traffic forwarded from the relay's address.

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

type Assist = (SocketAddr, Vec<u8>);

/// A full inbox sheds: assist is best-effort and must not backpressure quinn's receive path.
const INBOX_CAP: usize = 2048;

/// Assist datagrams peeled per `poll_recv` before the poll yields, so a
/// stream of assist traffic can't monopolise quinn's receive path.
const MAX_PEEL_PER_POLL: usize = 16;

/// The QUIC socket, with assist datagrams split off before quinn sees them.
#[derive(Debug)]
pub struct AssistSocket {
    io:    Arc<UdpSocket>,
    inbox: mpsc::Sender<Assist>,
}

pub struct AssistInbox {
    rx:   mpsc::Receiver<Assist>,
    sock: Arc<UdpSocket>,
}

impl std::fmt::Debug for AssistInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistInbox").finish_non_exhaustive()
    }
}

/// Must run inside the tokio runtime: the socket registers with the reactor.
pub fn wrap_socket(std_sock: std::net::UdpSocket) -> io::Result<(Arc<AssistSocket>, AssistInbox)> {
    std_sock.set_nonblocking(true)?;
    let io = Arc::new(UdpSocket::from_std(std_sock)?);
    let (inbox, rx) = mpsc::channel(INBOX_CAP);
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
        for _ in 0..MAX_PEEL_PER_POLL {
            let (len, src) = {
                let mut rb = tokio::io::ReadBuf::new(&mut bufs[0]);
                match self.io.poll_recv_from(cx, &mut rb) {
                    Poll::Ready(Ok(src)) => (rb.filled().len(), src),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            };
            if is_assist(&bufs[0][..len]) {
                let _ = self.inbox.try_send((src, bufs[0][..len].to_vec()));
                continue;
            }
            meta[0] = udp::RecvMeta { addr: src, len, stride: len, ecn: None, dst_ip: None };
            return Poll::Ready(Ok(1));
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }
}

#[derive(Debug)]
struct AssistPoller {
    io: Arc<UdpSocket>,
}

impl UdpPoller for AssistPoller {
    fn poll_writable(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        self.io.poll_send_ready(cx)
    }
}

const IDLE_TTL: Duration = Duration::from_secs(60);
const SWEEP: Duration = Duration::from_secs(30);
const MAX_BRIDGES: usize = 4096;

/// The two ends of one bridge, learned from their datagrams' source addresses.
struct Bridge {
    a:    SocketAddr,
    b:    Option<SocketAddr>,
    seen: Instant,
}

impl Bridge {
    /// The far side of `src`, registering it as the second end if that is free. `None` for a
    /// third source on this token.
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
            }
        },
        Some(RelayMsg::TurnData { token, .. }) => {
            let now = Instant::now();
            let dst = get_or_insert(bridges, token, src, now).and_then(|br| br.other(src, now));
            if let Some(dst) = dst {
                let _ = sock.send_to(pkt, dst).await;
            }
        },
        Some(RelayMsg::StunResp { .. }) | None => {},
    }
}

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
