//! Optional phone-to-node listener. Every TLS pipe owns exactly one inner
//! QUIC connection, with the original TCP addresses and existing client
//! handler. It cannot create a UDP proxy or register another network node.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use quinn::{Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::config::build_server_cfg;
use super::protorole::ProtoRole;
use super::tunnel::{self, AcceptedMode, Channel};
use crate::node::config::NetworkConfig;

const MAX_LIVE: usize = 1024;
const MAX_SOURCE_BUCKETS: usize = 4096;
const ACCEPT_BURST: f64 = 60.0;
const ACCEPT_PER_SECOND: f64 = 2.0;
const INNER_HANDSHAKE: Duration = Duration::from_secs(8);

/// Construct before starting the daemon so an occupied TCP port is a startup
/// error, rather than an advertised fallback which silently never listens.
pub struct NodeTunnel {
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    inner: ServerConfig,
    features: u64,
}

impl NodeTunnel {
    pub async fn bind(network: &NetworkConfig, features: u64) -> Result<Option<Self>> {
        if !network.tcp_fallback {
            return Ok(None);
        }
        super::config::setup_crypto_provider()?;
        let tls = tunnel::server_config(&network.cert_path, &network.key_path)?;
        // Keep node roles out even though the existing handler also serves
        // them on its native UDP endpoint.
        let mut inner =
            build_server_cfg(&network.cert_path, &network.key_path, &[ProtoRole::Client])?;
        inner.max_incoming(1);
        let listener = TcpListener::bind(network.bind_addr())
            .await
            .context("binding TLS fallback listener")?;
        Ok(Some(Self { listener, tls, inner, features }))
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub fn spawn<C, CF, A, AF>(self, control: C, assist: A) -> Running
    where
        C: Fn(Connection) -> CF + Send + Sync + 'static,
        CF: Future<Output = ()> + Send + 'static,
        A: Fn(Arc<Channel>, AcceptedMode) -> AF + Send + Sync + 'static,
        AF: Future<Output = ()> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let child = cancel.clone();
        let task = tokio::spawn(self.run(Arc::new(control), Arc::new(assist), child));
        Running { cancel, task: Some(task) }
    }

    async fn run<C, CF, A, AF>(self, control: Arc<C>, assist: Arc<A>, cancel: CancellationToken)
    where
        C: Fn(Connection) -> CF + Send + Sync + 'static,
        CF: Future<Output = ()> + Send + 'static,
        A: Fn(Arc<Channel>, AcceptedMode) -> AF + Send + Sync + 'static,
        AF: Future<Output = ()> + Send + 'static,
    {
        let slots = Arc::new(Semaphore::new(MAX_LIVE));
        let mut limits = Sources::default();
        let mut tasks = JoinSet::new();
        let mut sweep = tokio::time::interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = sweep.tick() => limits.sweep(Instant::now()),
                _ = tasks.join_next(), if !tasks.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, source) = match accepted {
                        Ok(pair) => pair,
                        Err(error) => {
                            log::warn!("TLS fallback accept failed: {error}");
                            break;
                        },
                    };
                    if !limits.admit(source.ip(), Instant::now()) { continue; }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                    let tls = self.tls.clone();
                    let inner = self.inner.clone();
                    let features = self.features;
                    let control = control.clone();
                    let assist = assist.clone();
                    let child = cancel.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let accepted = tokio::select! {
                            _ = child.cancelled() => return,
                            accepted = tunnel::accept(stream, tls, features) => accepted,
                        };
                        let Ok(accepted) = accepted else { return; };
                        let channel = accepted.channel;
                        let _close = CloseChannel(channel.clone());
                        match accepted.mode {
                            AcceptedMode::Control => {
                                if let Err(error) = serve_control(channel, inner, control, child).await {
                                    log::debug!("TLS fallback control ended: {error}");
                                }
                            },
                            mode @ AcceptedMode::Assist { .. } => {
                                tokio::select! {
                                    _ = child.cancelled() => {},
                                    _ = assist(channel, mode) => {},
                                }
                            },
                        }
                    });
                },
            }
        }
        cancel.cancel();
        if tokio::time::timeout(Duration::from_secs(5), async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
}

pub struct Running {
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Running {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct CloseChannel(Arc<Channel>);
impl Drop for CloseChannel {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct CloseEndpoint(Endpoint);
impl Drop for CloseEndpoint {
    fn drop(&mut self) {
        self.0.close(0u32.into(), b"TLS pipe closed");
    }
}

async fn serve_control<C, CF>(
    channel: Arc<Channel>, config: ServerConfig, handle: Arc<C>, cancel: CancellationToken,
) -> Result<()>
where
    C: Fn(Connection) -> CF + Send + Sync + 'static,
    CF: Future<Output = ()> + Send + 'static,
{
    let socket = channel.clone().socket(channel.local_addr(), channel.peer_addr());
    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(config),
        socket,
        Arc::new(TokioRuntime),
    )?;
    let _endpoint = CloseEndpoint(endpoint.clone());
    let connection = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        _ = channel.closed() => return Ok(()),
        connected = tokio::time::timeout(INNER_HANDSHAKE, async {
            let incoming = endpoint.accept().await.context("inner endpoint closed")?;
            // Incoming::accept consumes the server configuration. Establish
            // that connecting state before disabling future admission.
            let connecting = incoming.accept()?;
            // No second connection on one anonymous pipe. The TLS admission
            // permit therefore bounds inner connections too.
            endpoint.set_server_config(None);
            Ok::<_, anyhow::Error>(connecting.await?)
        }) => connected??,
    };
    let mut handler = tokio::spawn(handle(connection.clone()));
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = channel.closed() => {},
        _ = connection.closed() => {},
        _ = &mut handler => return Ok(()),
    }
    connection.close(0u32.into(), b"TLS pipe closed");
    // Existing relay handlers must run their generation-safe client/presence
    // cleanup. Dropping the future on TLS EOF would bypass it.
    if tokio::time::timeout(Duration::from_secs(2), &mut handler).await.is_err() {
        handler.abort();
        let _ = handler.await;
    }
    Ok(())
}

#[derive(Default)]
struct Sources(HashMap<IpAddr, Bucket>);
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Sources {
    fn sweep(&mut self, now: Instant) {
        self.0.retain(|_, bucket| {
            now.saturating_duration_since(bucket.updated) < Duration::from_secs(60)
        });
    }

    fn admit(&mut self, ip: IpAddr, now: Instant) -> bool {
        // IPv4-mapped IPv6 and native IPv4 are one source for quotas.
        let ip = match ip {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(ip)),
            ip => ip,
        };
        if !self.0.contains_key(&ip) && self.0.len() >= MAX_SOURCE_BUCKETS {
            self.sweep(now);
            if self.0.len() >= MAX_SOURCE_BUCKETS {
                return false;
            }
        }
        let bucket = self.0.entry(ip).or_insert(Bucket { tokens: ACCEPT_BURST, updated: now });
        bucket.tokens = (bucket.tokens
            + now.saturating_duration_since(bucket.updated).as_secs_f64() * ACCEPT_PER_SECOND)
            .min(ACCEPT_BURST);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_sources_have_independent_bounded_budgets() {
        let mut sources = Sources::default();
        let a = "192.0.2.1".parse().unwrap();
        let mapped = "::ffff:192.0.2.1".parse().unwrap();
        let b = "192.0.2.2".parse().unwrap();
        let now = Instant::now();
        for _ in 0..ACCEPT_BURST as usize {
            assert!(sources.admit(a, now));
        }
        assert!(!sources.admit(mapped, now));
        assert!(sources.admit(b, now));
        assert!(sources.admit(a, now + Duration::from_secs(1)));
        sources.sweep(now + Duration::from_secs(62));
        assert!(sources.0.is_empty());
    }

    #[test]
    fn rotating_source_addresses_cannot_grow_the_limiter_without_bound() {
        let mut sources = Sources::default();
        let now = Instant::now();
        for n in 0..MAX_SOURCE_BUCKETS as u32 {
            assert!(sources.admit(std::net::Ipv4Addr::from(n).into(), now));
        }
        assert!(!sources.admit("203.0.113.1".parse().unwrap(), now));
        assert_eq!(sources.0.len(), MAX_SOURCE_BUCKETS);
        assert!(sources.admit("203.0.113.1".parse().unwrap(), now + Duration::from_secs(61)));
    }

    async fn test_listener() -> (NodeTunnel, rustls::RootCertStore) {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = cert.der().clone();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let mut tls =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key.clone_key().into())
                .unwrap();
        tls.alpn_protocols = vec![tunnel::ALPN.to_vec()];
        let mut inner =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert], key.into())
                .unwrap();
        inner.alpn_protocols = vec![ProtoRole::Client.alpn().into_bytes()];
        let mut inner = ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(inner).unwrap(),
        ));
        inner.max_incoming(1);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        (
            NodeTunnel { listener, tls: Arc::new(tls), inner, features: tunnel::FEATURE_CONTROL },
            roots,
        )
    }

    /// Real TLS and QUIC, with no UDP socket on either endpoint. In
    /// particular node-role ALPN cannot reach the reused multi-role handler.
    #[tokio::test]
    async fn control_pipe_preserves_source_address_and_excludes_node_roles() {
        let (listener, roots) = test_listener().await;
        let address = listener.listener.local_addr().unwrap();
        let (accepted, mut incoming) = tokio::sync::mpsc::channel(2);
        let running = listener.spawn(
            move |connection| {
                let accepted = accepted.clone();
                async move {
                    accepted.send(connection.clone()).await.unwrap();
                    connection.closed().await;
                }
            },
            |channel, _| async move {
                channel.close();
                panic!("control-only listener admitted assist");
            },
        );
        let channel =
            tunnel::connect(address, "localhost", &roots, tunnel::Request::Control).await.unwrap();
        let mut endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            channel.clone().socket(channel.local_addr(), channel.peer_addr()),
            Arc::new(TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(
            super::super::config::build_client_cfg(ProtoRole::Client, &roots).unwrap(),
        );
        let client = tokio::time::timeout(
            Duration::from_secs(5),
            endpoint.connect(address, "localhost").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        let server =
            tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.unwrap().unwrap();
        assert_eq!(server.remote_address(), channel.local_addr());
        let (mut send, mut recv) = client.open_bi().await.unwrap();
        send.write_all(b"opaque control request").await.unwrap();
        send.finish().unwrap();
        let (mut reply, mut request) = server.accept_bi().await.unwrap();
        assert_eq!(request.read_to_end(1024).await.unwrap(), b"opaque control request");
        reply.write_all(b"opaque control response").await.unwrap();
        reply.finish().unwrap();
        assert_eq!(recv.read_to_end(1024).await.unwrap(), b"opaque control response");
        channel.close();
        tokio::time::timeout(Duration::from_secs(3), server.closed())
            .await
            .expect("TLS EOF must close inner connection promptly");

        let channel =
            tunnel::connect(address, "localhost", &roots, tunnel::Request::Control).await.unwrap();
        let mut endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            channel.clone().socket(channel.local_addr(), channel.peer_addr()),
            Arc::new(TokioRuntime),
        )
        .unwrap();
        endpoint.set_default_client_config(
            super::super::config::build_client_cfg(ProtoRole::Relay, &roots).unwrap(),
        );
        let denied = tokio::time::timeout(
            Duration::from_secs(5),
            endpoint.connect(address, "localhost").unwrap(),
        )
        .await
        .unwrap();
        assert!(denied.is_err(), "node registration ALPN must not enter through the phone tunnel");
        assert!(incoming.try_recv().is_err());
        running.shutdown().await;
    }
}
