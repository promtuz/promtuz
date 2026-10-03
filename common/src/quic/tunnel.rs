//! Bounded opaque datagrams over server-authenticated TLS/TCP. Control discloses no identity here
//! (inner QUIC authenticates); assist proves IPK ownership against the fresh TLS exporter.

use anyhow::{Result, anyhow};
use ed25519_dalek::{Signature, VerifyingKey};
use quinn::{AsyncUdpSocket, UdpPoller, udp};
use rustls::{RootCertStore, pki_types::ServerName};
use std::{
    fmt, io,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::task::JoinHandle;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_util::sync::{CancellationToken, PollSender};

pub const ALPN: &[u8] = b"promtuz-tunnel/1";
pub const FEATURE_CONTROL: u64 = 1;
pub const FEATURE_ASSIST: u64 = 2;
pub const MAX_PACKET: usize = 2048;
pub const QUEUE_PACKETS: usize = 64;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const EXPORTER_LABEL: &[u8] = b"promtuz-tunnel-registration-v1";
const SIGNING_PREFIX: &[u8] = b"promtuz-tunnel-assist-v1\0";

/// Certificate, authenticated negotiation or identity validation failure.
/// Callers must not weaken authentication or retry another grammar on it.
#[derive(Debug, thiserror::Error)]
#[error("tunnel security or protocol failure: {0}")]
pub struct SecurityError(pub String);

fn invalid(message: impl Into<String>) -> anyhow::Error {
    SecurityError(message.into()).into()
}

pub type Sign = Arc<dyn Fn(&[u8]) -> Result<[u8; 64]> + Send + Sync>;

pub enum Request {
    Control,
    Assist { token: [u8; 16], ipk: [u8; 32], peer: [u8; 32], sign: Sign },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptedMode {
    Control,
    Assist { token: [u8; 16], ipk: [u8; 32], peer: [u8; 32] },
}

pub struct Accepted {
    pub channel: Arc<Channel>,
    pub mode: AcceptedMode,
}

fn tls_error(error: io::Error) -> anyhow::Error {
    if error.get_ref().is_some_and(|source| source.is::<rustls::Error>()) {
        invalid(error.to_string())
    } else {
        error.into()
    }
}

pub fn server_config(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>> {
    let mut certs = io::BufReader::new(std::fs::File::open(cert)?);
    let certs = rustls_pemfile::certs(&mut certs).collect::<std::result::Result<Vec<_>, _>>()?;
    let mut key = io::BufReader::new(std::fs::File::open(key)?);
    let key = rustls_pemfile::private_key(&mut key)?.ok_or_else(|| anyhow!("missing TLS key"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

fn client_config(roots: &RootCertStore) -> Result<Arc<rustls::ClientConfig>> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_root_certificates(roots.clone())
    .with_no_client_auth();
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Arc::new(config))
}

fn check_alpn(alpn: Option<&[u8]>) -> Result<()> {
    if alpn != Some(ALPN) {
        return Err(invalid("unexpected tunnel ALPN"));
    }
    Ok(())
}

fn ready(features: u64) -> Vec<u8> {
    let mut bytes = vec![1];
    bytes.extend_from_slice(&features.to_le_bytes());
    bytes.extend_from_slice(&(MAX_PACKET as u16).to_le_bytes());
    bytes
}

fn parse_ready(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != 11
        || bytes[0] != 1
        || u16::from_le_bytes(bytes[9..11].try_into().unwrap()) as usize != MAX_PACKET
    {
        return Err(invalid("invalid tunnel Ready"));
    }
    Ok(u64::from_le_bytes(bytes[1..9].try_into().unwrap()))
}

fn assist_message(
    exporter: &[u8; 32], features: u64, token: &[u8; 16], ipk: &[u8; 32], peer: &[u8; 32],
) -> Vec<u8> {
    [SIGNING_PREFIX, exporter, &ready(features), token, ipk, peer].concat()
}

fn encode_request(request: Request, exporter: &[u8; 32], features: u64) -> Result<Vec<u8>> {
    match request {
        Request::Control => {
            if features & FEATURE_CONTROL == 0 {
                return Err(invalid("control tunnel unsupported"));
            }
            Ok(vec![2])
        },
        Request::Assist { token, ipk, peer, sign } => {
            if features & FEATURE_ASSIST == 0 || ipk == peer {
                return Err(invalid("assist tunnel unsupported or self-addressed"));
            }
            let sig = sign(&assist_message(exporter, features, &token, &ipk, &peer))?;
            let mut out = vec![3];
            out.extend_from_slice(&token);
            out.extend_from_slice(&ipk);
            out.extend_from_slice(&peer);
            out.extend_from_slice(&sig);
            Ok(out)
        },
    }
}

fn decode_request(bytes: &[u8], exporter: &[u8; 32], features: u64) -> Result<AcceptedMode> {
    match bytes.first() {
        Some(2) if bytes.len() == 1 && features & FEATURE_CONTROL != 0 => Ok(AcceptedMode::Control),
        Some(3) if bytes.len() == 145 && features & FEATURE_ASSIST != 0 => {
            let token = bytes[1..17].try_into().unwrap();
            let ipk = bytes[17..49].try_into().unwrap();
            let peer = bytes[49..81].try_into().unwrap();
            if ipk == peer {
                return Err(invalid("self-addressed assist route"));
            }
            let sig = Signature::from_bytes(bytes[81..145].try_into().unwrap());
            VerifyingKey::from_bytes(&ipk)
                .map_err(|_| invalid("invalid assist identity"))?
                .verify_strict(&assist_message(exporter, features, &token, &ipk, &peer), &sig)
                .map_err(|_| invalid("assist identity proof failed"))?;
            Ok(AcceptedMode::Assist { token, ipk, peer })
        },
        _ => Err(invalid("invalid or unsupported tunnel request")),
    }
}

/// Length is validated before allocating. Handshake phases use their own
/// smaller ceiling; a datagram body cannot be smuggled into registration.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Vec<u8>> {
    let len = r.read_u16().await? as usize;
    if len == 0 || len > max {
        return Err(invalid("invalid tunnel frame length"));
    }
    let mut bytes = vec![0; len];
    r.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() || bytes.len() > MAX_PACKET {
        return Err(invalid("invalid tunnel packet length"));
    }
    w.write_u16(bytes.len() as u16).await?;
    w.write_all(bytes).await?;
    w.flush().await?;
    Ok(())
}

pub async fn connect(
    addr: SocketAddr, server_name: &str, roots: &RootCertStore, request: Request,
) -> Result<Arc<Channel>> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let stream = TcpStream::connect(addr).await?;
        stream.set_nodelay(true)?;
        let local = stream.local_addr()?;
        let remote = stream.peer_addr()?;
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|_| invalid("invalid tunnel server name"))?;
        let mut tls = TlsConnector::from(client_config(roots)?)
            .connect(name, stream)
            .await
            .map_err(tls_error)?;
        check_alpn(tls.get_ref().1.alpn_protocol())?;
        let exporter = tls
            .get_ref()
            .1
            .export_keying_material([0u8; 32], EXPORTER_LABEL, None)
            .map_err(|_| invalid("tunnel exporter unavailable"))?;
        let features = parse_ready(&read_frame(&mut tls, 11).await?)?;
        let request = encode_request(request, &exporter, features)?;
        write_frame(&mut tls, &request).await?;
        if read_frame(&mut tls, 1).await? != [4] {
            return Err(invalid("tunnel registration rejected"));
        }
        Ok(Channel::start(tls, local, remote, features))
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS tunnel setup timed out"))?
}

pub async fn accept(
    stream: TcpStream, config: Arc<rustls::ServerConfig>, features: u64,
) -> Result<Accepted> {
    timeout(HANDSHAKE_TIMEOUT, async {
        stream.set_nodelay(true)?;
        let local = stream.local_addr()?;
        let remote = stream.peer_addr()?;
        let mut tls = TlsAcceptor::from(config).accept(stream).await.map_err(tls_error)?;
        check_alpn(tls.get_ref().1.alpn_protocol())?;
        let exporter = tls
            .get_ref()
            .1
            .export_keying_material([0u8; 32], EXPORTER_LABEL, None)
            .map_err(|_| invalid("tunnel exporter unavailable"))?;
        write_frame(&mut tls, &ready(features)).await?;
        let bytes = read_frame(&mut tls, 145).await?;
        let mode = match decode_request(&bytes, &exporter, features) {
            Ok(mode) => mode,
            Err(error) => {
                // Make an explicit proof/protocol rejection distinguishable
                // from a network loss at the dialing endpoint.
                let _ = write_frame(&mut tls, &[5]).await;
                return Err(error);
            },
        };
        write_frame(&mut tls, &[4]).await?;
        Ok(Accepted { channel: Channel::start(tls, local, remote, features), mode })
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "TLS tunnel setup timed out"))?
}

/// A single-reader bounded datagram pipe. The driver owns no `Arc<Channel>`, so
/// dropping the last route/socket owner cancels both I/O halves immediately.
pub struct Channel {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    closed: CancellationToken,
    local: SocketAddr,
    remote: SocketAddr,
    features: u64,
    driver: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    finished: CancellationToken,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelChannel")
            .field("closed", &self.closed.is_cancelled())
            .finish_non_exhaustive()
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl Channel {
    fn start<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
        stream: S, local: SocketAddr, remote: SocketAddr, features: u64,
    ) -> Arc<Self> {
        let (tx, mut outgoing) = mpsc::channel::<Vec<u8>>(QUEUE_PACKETS);
        let (incoming, rx) = mpsc::channel(QUEUE_PACKETS);
        let closed = CancellationToken::new();
        let cancel = closed.clone();
        let finished = CancellationToken::new();
        let done = finished.clone();
        let driver = tokio::spawn(async move {
            struct Finished(CancellationToken);
            impl Drop for Finished {
                fn drop(&mut self) {
                    self.0.cancel();
                }
            }
            let _finished = Finished(done);
            let (mut reader, mut writer) = tokio::io::split(stream);
            let read = async {
                loop {
                    let bytes =
                        timeout(READ_TIMEOUT, read_frame(&mut reader, MAX_PACKET)).await??;
                    // A caller that has stopped draining cannot pin a TLS task
                    // forever. Unrelated pipes have independent workers/queues.
                    timeout(WRITE_TIMEOUT, incoming.send(bytes))
                        .await?
                        .map_err(|_| anyhow!("tunnel receiver closed"))?;
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let write = async {
                while let Some(bytes) = outgoing.recv().await {
                    timeout(WRITE_TIMEOUT, write_frame(&mut writer, &bytes)).await??;
                }
                Ok::<(), anyhow::Error>(())
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = read => {},
                _ = write => {},
            }
            cancel.cancel();
        });
        Arc::new(Self {
            tx,
            rx: Mutex::new(rx),
            closed,
            local,
            remote,
            features,
            driver: tokio::sync::Mutex::new(Some(driver)),
            finished,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
    pub fn peer_addr(&self) -> SocketAddr {
        self.remote
    }
    pub fn features(&self) -> u64 {
        self.features
    }
    pub fn close(&self) {
        self.closed.cancel();
    }
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }
    /// Close and join the owned IO driver. Concurrent shutdown callers all
    /// wait for completion; signalling cancellation alone is not a join.
    pub async fn shutdown(&self) -> Result<()> {
        self.close();
        let mut driver = self.driver.lock().await;
        if let Some(task) = driver.as_mut() {
            // Keep the handle stored while awaiting: dropping this future
            // must not detach ownership from a later shutdown caller.
            let result = task.await;
            driver.take();
            // An aborted task might never have been polled, before its guard
            // was constructed. Join completion is authoritative in that case.
            self.finished.cancel();
            result?;
        } else {
            self.finished.cancelled().await;
        }
        Ok(())
    }
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    pub fn try_send(&self, bytes: &[u8]) -> io::Result<()> {
        if self.is_closed() {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if bytes.is_empty() || bytes.len() > MAX_PACKET {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let permit = self.tx.try_reserve().map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
            mpsc::error::TrySendError::Closed(_) => io::Error::from(io::ErrorKind::BrokenPipe),
        })?;
        permit.send(bytes.to_vec());
        Ok(())
    }

    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<Vec<u8>>> {
        if self.is_closed() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        match self.rx.lock().unwrap().poll_recv(cx) {
            Poll::Ready(Some(packet)) => Poll::Ready(Ok(packet)),
            Poll::Ready(None) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            Poll::Pending => Poll::Pending,
        }
    }
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
    pub fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        Box::pin(QueuePoller {
            sender: PollSender::new(self.tx.clone()),
            closed: self.closed.clone(),
        })
    }
    pub fn socket(
        self: Arc<Self>, local: SocketAddr, remote: SocketAddr,
    ) -> Arc<dyn AsyncUdpSocket> {
        Arc::new(TunnelSocket { channel: self, local, remote })
    }
}

#[derive(Debug)]
struct QueuePoller {
    sender: PollSender<Vec<u8>>,
    closed: CancellationToken,
}
impl UdpPoller for QueuePoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.closed.is_cancelled() {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        match self.sender.poll_reserve(cx) {
            Poll::Ready(Ok(())) => {
                // Readiness reserves capacity only for this poll. try_send is
                // the single commit point; a racing sender can make it retry.
                self.sender.abort_send();
                Poll::Ready(Ok(()))
            },
            Poll::Ready(Err(_)) => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug)]
struct TunnelSocket {
    channel: Arc<Channel>,
    local: SocketAddr,
    remote: SocketAddr,
}
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}
impl AsyncUdpSocket for TunnelSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.channel.clone().create_io_poller()
    }
    fn try_send(&self, transmit: &udp::Transmit) -> io::Result<()> {
        if canonical(transmit.destination) != canonical(self.remote)
            || transmit.segment_size.is_some()
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.channel.try_send(transmit.contents)
    }
    fn poll_recv(
        &self, cx: &mut Context<'_>, bufs: &mut [io::IoSliceMut<'_>], meta: &mut [udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let packet = match self.channel.poll_recv(cx) {
            Poll::Ready(Ok(packet)) => packet,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        };
        if bufs.is_empty() || meta.is_empty() || packet.len() > bufs[0].len() {
            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
        }
        bufs[0][..packet.len()].copy_from_slice(&packet);
        meta[0] = udp::RecvMeta {
            addr: self.remote,
            len: packet.len(),
            stride: packet.len(),
            ecn: None,
            dst_ip: None,
        };
        Poll::Ready(Ok(1))
    }
    fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.local)
    }
    fn max_transmit_segments(&self) -> usize {
        1
    }
    fn max_receive_segments(&self) -> usize {
        1
    }
    fn may_fragment(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcripts() {
        crate::proto::golden(
            &[assist_message(&[1; 32], 0x0102, &[2; 16], &[3; 32], &[4; 32])],
            "525a288d37a9b4011c8ae4828725990e06e3498911699c6f1adc794e92fb9f65",
        );
    }
}
