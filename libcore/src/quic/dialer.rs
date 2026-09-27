use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::node::config::DEFAULT_RESOLVER_PORT;
use common::quic::protorole::ProtoRole;
use common::quic::tunnel;
use once_cell::sync::OnceCell;
use quinn::{Connection, ConnectionError, Endpoint};
use thiserror::Error;

use crate::ENDPOINT;
use crate::data::ResolverSeed;

const UDP_HEAD_START: Duration = Duration::from_secs(2);
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

struct ClientConfigs {
    quic: quinn::ClientConfig,
    roots: rustls::RootCertStore,
}

static CONFIGS: OnceCell<ClientConfigs> = OnceCell::new();

pub(crate) fn initialize(
    quic: quinn::ClientConfig, roots: rustls::RootCertStore,
) -> anyhow::Result<()> {
    CONFIGS
        .set(ClientConfigs { quic, roots })
        .map_err(|_| anyhow::anyhow!("node dialer initialized twice"))
}

pub(crate) fn roots() -> anyhow::Result<&'static rustls::RootCertStore> {
    Ok(&CONFIGS.get().ok_or_else(|| anyhow::anyhow!("node dialer not initialized"))?.roots)
}

pub fn quinn_err<E>(e: E) -> DialerError
where
    E: std::error::Error + Send + Sync + 'static,
{
    DialerError::Quinn(Box::new(e))
}

#[derive(Error, Debug)]
pub enum DialerError {
    #[error("quinn failure: {0}")]
    Quinn(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to connect: {0}")]
    Error(#[from] io::Error),

    #[error("node authentication or protocol failed: {0}")]
    Security(#[source] anyhow::Error),
}

impl DialerError {
    pub(crate) fn is_security(&self) -> bool {
        matches!(self, Self::Security(_))
    }
}

/// Only loss or a reset permits changing carriers. A TLS alert, a malformed
/// QUIC peer or an explicit rejection must not trigger a second handshake.
fn permits_fallback(error: &ConnectionError) -> bool {
    matches!(error, ConnectionError::TimedOut | ConnectionError::Reset)
}

fn quic_failure(error: ConnectionError) -> DialerError {
    match error {
        ConnectionError::TransportError(_)
        | ConnectionError::ConnectionClosed(_)
        | ConnectionError::ApplicationClosed(_)
        | ConnectionError::VersionMismatch => DialerError::Security(error.into()),
        other => quinn_err(other),
    }
}

fn validate_protocol(conn: &Connection) -> Result<(), DialerError> {
    let expected = ProtoRole::Client.alpn();
    let matches = conn
        .handshake_data()
        .and_then(|data| {
            data.downcast_ref::<quinn::crypto::rustls::HandshakeData>()
                .map(|data| data.protocol.as_deref() == Some(expected.as_bytes()))
        })
        .unwrap_or(false);
    if !matches {
        conn.close(0u32.into(), b"unexpected node protocol");
        return Err(DialerError::Security(anyhow::anyhow!("unexpected node ALPN")));
    }
    Ok(())
}

/// Phone-to-node dialing shared by discovery, messaging, push registration and
/// store uploads. The outer Control pipe reveals no user identity: the existing
/// inner protocol retains its own authentication and privacy boundary.
pub(crate) async fn connect(addr: SocketAddr, name: &str) -> Result<Connection, DialerError> {
    let endpoint = ENDPOINT.get().ok_or_else(|| io::Error::other("endpoint not initialized"))?;
    let configs = CONFIGS.get().ok_or_else(|| io::Error::other("node dialer not initialized"))?;
    connect_with(endpoint, configs, addr, name).await
}

async fn connect_with(
    endpoint: &Endpoint, configs: &ClientConfigs, addr: SocketAddr, name: &str,
) -> Result<Connection, DialerError> {
    let connecting = endpoint.connect_with(configs.quic.clone(), addr, name).map_err(quinn_err)?;
    let udp = async {
        let conn = connecting.await.map_err(quic_failure)?;
        validate_protocol(&conn)?;
        Ok(conn)
    };
    tokio::pin!(udp);
    match tokio::time::timeout(UDP_HEAD_START, &mut udp).await {
        Ok(Ok(conn)) => {
            return Ok(conn);
        },
        Ok(Err(error)) => {
            let may_retry = match &error {
                DialerError::Quinn(source) => {
                    source.downcast_ref::<ConnectionError>().is_some_and(permits_fallback)
                },
                _ => false,
            };
            if !may_retry {
                return Err(error);
            }
            return tokio::time::timeout(
                TUNNEL_CONNECT_TIMEOUT,
                connect_tunnel(configs, addr, name),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "node TLS fallback timed out"))?;
        },
        Err(_) => {},
    }
    // A slow but valid UDP-only node must retain the original connect budget.
    // The head start enables TCP; it does not cancel the pending UDP handshake.
    tokio::time::timeout(
        TUNNEL_CONNECT_TIMEOUT,
        race_transports(&mut udp, connect_tunnel(configs, addr, name)),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "node connection timed out"))?
}

async fn race_transports(
    udp: impl std::future::Future<Output = Result<Connection, DialerError>>,
    tcp: impl std::future::Future<Output = Result<Connection, DialerError>>,
) -> Result<Connection, DialerError> {
    tokio::pin!(udp, tcp);
    let mut udp_done = false;
    let mut tcp_done = false;
    loop {
        let (is_udp, result) = tokio::select! {
            biased;
            result = &mut udp, if !udp_done => (true, result),
            result = &mut tcp, if !tcp_done => (false, result),
        };
        if is_udp {
            udp_done = true;
        } else {
            tcp_done = true;
        }
        let other = if result.is_ok() || result.as_ref().is_err_and(|error| error.is_security()) {
            if is_udp && !tcp_done {
                ready_result(tcp.as_mut()).await
            } else if !is_udp && !udp_done {
                ready_result(udp.as_mut()).await
            } else {
                None
            }
        } else {
            None
        };
        match result {
            Err(error) if error.is_security() => {
                if let Some(Ok(other)) = other {
                    other.close(0u32.into(), b"other node carrier rejected");
                }
                return Err(error);
            },
            Err(error) => {
                if udp_done && tcp_done {
                    return Err(error);
                }
            },
            Ok(conn) => {
                // A simultaneously-ready rejection from the other carrier
                // cannot be hidden by select ordering. Pending losers are
                // cancelled; successful losers must be explicitly closed.
                match other {
                    Some(Err(error)) if error.is_security() => {
                        conn.close(0u32.into(), b"other node carrier rejected");
                        return Err(error);
                    },
                    Some(Ok(other)) => other.close(0u32.into(), b"node carrier not selected"),
                    _ => {},
                }
                return Ok(conn);
            },
        }
    }
}

async fn ready_result<F: std::future::Future>(
    mut future: std::pin::Pin<&mut F>,
) -> Option<F::Output> {
    std::future::poll_fn(|cx| {
        std::task::Poll::Ready(match future.as_mut().poll(cx) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        })
    })
    .await
}

/// Own both transport layers during setup and after handoff. Cancelling any
/// intermediate await closes the private endpoint and pipe; late workers can
/// never touch a replacement connection.
struct PipeGuard {
    channel: Arc<tunnel::Channel>,
    endpoint: Option<Endpoint>,
}

impl Drop for PipeGuard {
    fn drop(&mut self) {
        if let Some(endpoint) = &self.endpoint {
            endpoint.close(0u32.into(), b"node tunnel closed");
        }
        self.channel.close();
    }
}

async fn connect_tunnel(
    configs: &ClientConfigs, addr: SocketAddr, name: &str,
) -> Result<Connection, DialerError> {
    let channel = tunnel::connect(addr, name, &configs.roots, tunnel::Request::Control)
        .await
        .map_err(|error| {
            if error.downcast_ref::<tunnel::SecurityError>().is_some() {
                DialerError::Security(error)
            } else {
                DialerError::Error(io::Error::other(error))
            }
        })?;
    let mut guard = PipeGuard { channel, endpoint: None };
    let socket =
        guard.channel.clone().socket(guard.channel.local_addr(), guard.channel.peer_addr());
    let endpoint = Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        None,
        socket,
        Arc::new(quinn::TokioRuntime),
    )?;
    guard.endpoint = Some(endpoint);
    let conn = guard
        .endpoint
        .as_ref()
        .unwrap()
        .connect_with(configs.quic.clone(), addr, name)
        .map_err(quinn_err)?
        .await
        .map_err(quic_failure)?;
    validate_protocol(&conn)?;
    tokio::spawn(async move {
        // Do not retain a Connection clone here: callers that fail during an
        // application handshake rely on dropping their last handle to close it.
        // The endpoint stays live until Quinn finishes that connection's drain.
        tokio::select! {
            _ = guard.endpoint.as_ref().unwrap().wait_idle() => {},
            _ = guard.channel.closed() => {},
        }
        drop(guard);
    });
    Ok(conn)
}

pub async fn connect_to_any_seed(seeds: &[ResolverSeed]) -> Result<Connection, DialerError> {
    let mut last_err: Option<DialerError> = None;

    for seed in seeds {
        // Resolve host[:port] -> SocketAddr at dial time (DNS + default port),
        // so a DNS repoint is picked up on reconnect. A resolve failure just
        // moves to the next seed rather than aborting the whole attempt.
        let addr = match seed.addr.resolve(DEFAULT_RESOLVER_PORT).await {
            Ok(a) => a,
            Err(err) => {
                log::error!("resolver {} resolve failed: {}", seed.addr, err);
                last_err = Some(err.into());
                continue;
            },
        };

        log::info!("connecting to resolver {} ({})", seed.addr, addr);

        match connect(addr, &seed.key.to_string()).await {
            Ok(conn) => {
                log::info!("connected to resolver {}", addr);
                return Ok(conn);
            },
            Err(err) => {
                log::error!("resolver {} connection failed: {}", addr, err);
                last_err = Some(err);
            },
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("no resolver seed succeeded").into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::proto::client_res::{ClientRequest, ClientResponse};
    use common::proto::pack::{Packer, Unpacker};
    use ed25519_dalek::SigningKey;

    fn tls_configs() -> (Arc<rustls::ServerConfig>, quinn::ServerConfig, ClientConfigs) {
        // Parallel fixtures may race to install the same process-wide provider.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
        let mut tls =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert.der().clone()], key.into())
                .unwrap();
        tls.alpn_protocols = vec![tunnel::ALPN.to_vec()];
        let mut inner_tls = tls.clone();
        inner_tls.alpn_protocols = vec![ProtoRole::Client.alpn().into_bytes()];
        let inner = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(inner_tls).unwrap(),
        ));
        let quic = common::quic::config::build_client_cfg(ProtoRole::Client, &roots).unwrap();
        (Arc::new(tls), inner, ClientConfigs { quic, roots })
    }

    fn peer_configs(alpn: &[u8]) -> (quinn::ServerConfig, quinn::ClientConfig) {
        crate::quic::peer_config::test_peer_configs_with_protocols(
            &SigningKey::from_bytes(&[199; 32]),
            vec![alpn.to_vec()],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn native_udp_node_still_serves_without_a_tcp_listener() {
        let (server_config, client_config) = peer_configs(ProtoRole::Client.alpn().as_bytes());
        let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let serving = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
            assert!(matches!(
                ClientRequest::unpack(&mut rx).await.unwrap(),
                ClientRequest::GetRelays()
            ));
            tx.write_all(&ClientResponse::GetRelays { relays: vec![] }.pack().unwrap())
                .await
                .unwrap();
            tx.finish().unwrap();
            conn.closed().await;
        });
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let configs = ClientConfigs { quic: client_config, roots: rustls::RootCertStore::empty() };
        let conn = connect_with(&endpoint, &configs, addr, "peer").await.unwrap();
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        tx.write_all(&ClientRequest::GetRelays().pack().unwrap()).await.unwrap();
        tx.finish().unwrap();
        assert!(matches!(ClientResponse::unpack(&mut rx).await.unwrap(),
            ClientResponse::GetRelays { relays } if relays.is_empty()));
        conn.close(0u32.into(), b"done");
        tokio::time::timeout(Duration::from_secs(2), serving).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn slow_udp_only_node_keeps_its_connect_budget_after_tcp_is_refused() {
        let (server_config, client_config) = peer_configs(ProtoRole::Client.alpn().as_bytes());
        let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        // The same numeric TCP port has no listener. An old UDP-only server
        // may still need retransmissions after the new TCP head start expires.
        drop(tokio::net::TcpListener::bind(addr).await.unwrap());
        let serving = tokio::spawn(async move {
            tokio::time::sleep(UDP_HEAD_START + Duration::from_millis(250)).await;
            let conn = server.accept().await.unwrap().await.unwrap();
            conn.closed().await;
        });
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let configs = ClientConfigs { quic: client_config, roots: rustls::RootCertStore::empty() };
        let started = tokio::time::Instant::now();
        let conn = tokio::time::timeout(
            Duration::from_secs(6),
            connect_with(&endpoint, &configs, addr, "peer"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() >= UDP_HEAD_START);
        conn.close(0u32.into(), b"done");
        tokio::time::timeout(Duration::from_secs(2), serving).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn either_carriers_ready_security_error_overrides_simultaneous_success() {
        for udp_failed in [false, true] {
            let (server_config, client_config) = peer_configs(ProtoRole::Client.alpn().as_bytes());
            let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let dial =
                client.connect_with(client_config, server.local_addr().unwrap(), "peer").unwrap();
            let (accepted, conn) =
                tokio::join!(async { server.accept().await.unwrap().await.unwrap() }, async {
                    dial.await.unwrap()
                },);
            let failure = Err(DialerError::Security(anyhow::anyhow!("rejected peer identity")));
            let success = Ok(conn.clone());
            let (udp, tcp) = if udp_failed { (failure, success) } else { (success, failure) };
            let result = race_transports(std::future::ready(udp), std::future::ready(tcp)).await;
            assert!(matches!(result, Err(DialerError::Security(_))));
            assert!(conn.close_reason().is_some(), "the successful loser must also close");
            accepted.close(0u32.into(), b"done");
        }
    }

    #[tokio::test]
    async fn rejected_certificate_and_wrong_negotiated_protocol_never_try_tcp() {
        for wrong_protocol in [false, true] {
            let alpn =
                if wrong_protocol { "unexpected/10".to_owned() } else { ProtoRole::Client.alpn() };
            let (server_config, permissive_client) = peer_configs(alpn.as_bytes());
            let server = Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            let tcp = tokio::net::TcpListener::bind(addr).await.unwrap();
            let serving = tokio::spawn(async move {
                if let Ok(conn) = server.accept().await.unwrap().await {
                    conn.closed().await;
                }
            });
            let roots = rustls::RootCertStore::empty();
            let quic = if wrong_protocol {
                permissive_client
            } else {
                common::quic::config::build_client_cfg(ProtoRole::Client, &roots).unwrap()
            };
            let configs = ClientConfigs { quic, roots };
            let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let result = connect_with(&endpoint, &configs, addr, "peer").await;
            assert!(matches!(result, Err(DialerError::Security(_))), "{result:?}");
            assert!(
                tokio::time::timeout(Duration::from_millis(50), tcp.accept()).await.is_err(),
                "a rejected UDP identity/protocol must not start a TCP handshake"
            );
            tokio::time::timeout(Duration::from_secs(2), serving).await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn udp_blackhole_falls_back_to_anonymous_tls_for_cold_discovery_and_closes_promptly() {
        use common::proto::client_res::RelayDescriptor;
        use common::quic::id::NodeId;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (tls, inner, configs) = tls_configs();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Consume and count every UDP Initial, deliberately returning nothing.
        // The successful RPC below therefore cannot have used native UDP.
        let udp = tokio::net::UdpSocket::bind(addr).await.unwrap();
        let udp_packets = Arc::new(AtomicUsize::new(0));
        let dropping = tokio::spawn({
            let udp_packets = udp_packets.clone();
            async move {
                let mut bytes = [0u8; 4096];
                while udp.recv_from(&mut bytes).await.is_ok() {
                    udp_packets.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let (closed_tx, close_rx) = tokio::sync::oneshot::channel();
        let (binding_tx, binding_rx) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            let accepted = tunnel::accept(stream, tls, tunnel::FEATURE_CONTROL).await.unwrap();
            assert_eq!(accepted.mode, tunnel::AcceptedMode::Control);
            let channel = accepted.channel;
            let socket = channel.clone().socket(channel.local_addr(), channel.peer_addr());
            let endpoint = Endpoint::new_with_abstract_socket(
                quinn::EndpointConfig::default(),
                Some(inner),
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap();
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            assert_eq!(conn.remote_address(), peer, "preserve the actual TCP origin");
            binding_tx.send(common::quic::client_auth_binding(&conn).unwrap()).unwrap();
            let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
            assert!(matches!(
                ClientRequest::unpack(&mut rx).await.unwrap(),
                ClientRequest::GetRelays()
            ));
            tx.write_all(
                &ClientResponse::GetRelays {
                    relays: vec![RelayDescriptor {
                        id: NodeId::from_bytes([71; 32]),
                        addr,
                        pubkey: [72; 32].into(),
                    }],
                }
                .pack()
                .unwrap(),
            )
            .await
            .unwrap();
            tx.finish().unwrap();
            close_rx.await.unwrap();
            channel.close();
            endpoint.close(0u32.into(), b"test relay restart");
        });
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let conn = connect_with(&endpoint, &configs, addr, "localhost").await.unwrap();
        assert!(udp_packets.load(Ordering::Relaxed) > 0, "UDP really was attempted and dropped");
        assert_eq!(
            common::quic::client_auth_binding(&conn).unwrap(),
            binding_rx.await.unwrap(),
            "the unchanged relay proof transcript must bind the same inner exporter on both sides"
        );
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        tx.write_all(&ClientRequest::GetRelays().pack().unwrap()).await.unwrap();
        tx.finish().unwrap();
        let response = ClientResponse::unpack(&mut rx).await.unwrap();
        assert!(matches!(response, ClientResponse::GetRelays { relays }
            if relays.len() == 1 && relays[0].id == NodeId::from_bytes([71; 32])));
        closed_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), conn.closed()).await.unwrap();
        serving.await.unwrap();
        dropping.abort();
    }

    #[tokio::test]
    async fn cancelling_after_tls_admission_closes_the_private_pipe() {
        let (tls, _inner, configs) = tls_configs();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dialing =
            tokio::spawn(async move { connect_tunnel(&configs, addr, "localhost").await });
        let (stream, _) = listener.accept().await.unwrap();
        let channel = tunnel::accept(stream, tls, tunnel::FEATURE_CONTROL).await.unwrap().channel;
        // First inner Initial proves the caller has constructed its private
        // endpoint/guard, rather than merely cancelling TCP establishment.
        tokio::time::timeout(Duration::from_secs(2), channel.recv()).await.unwrap().unwrap();
        dialing.abort();
        assert!(dialing.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), channel.closed()).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_a_connected_client_releases_its_pipe_without_explicit_close() {
        let (tls, inner, configs) = tls_configs();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (channel_tx, channel_rx) = tokio::sync::oneshot::channel();
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let channel =
                tunnel::accept(stream, tls, tunnel::FEATURE_CONTROL).await.unwrap().channel;
            let socket = channel.clone().socket(channel.local_addr(), channel.peer_addr());
            let endpoint = Endpoint::new_with_abstract_socket(
                quinn::EndpointConfig::default(),
                Some(inner),
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap();
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            channel_tx.send(channel.clone()).unwrap();
            channel.closed().await;
            endpoint.close(0u32.into(), b"test complete");
            conn.closed().await;
        });
        let conn = connect_tunnel(&configs, addr, "localhost").await.unwrap();
        let channel = channel_rx.await.unwrap();
        // No explicit Connection::close, exactly like an early-returning caller.
        drop(conn);
        tokio::time::timeout(Duration::from_secs(5), channel.closed()).await.unwrap();
        serving.await.unwrap();
    }
}
