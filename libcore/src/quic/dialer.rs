use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use common::node::config::DEFAULT_RESOLVER_PORT;
use common::quic::protorole::ProtoRole;
use common::quic::tunnel;
use quinn::{Connection, ConnectionError, Endpoint};
use thiserror::Error;

use crate::data::ResolverSeed;
use crate::state::core;

const UDP_HEAD_START: Duration = Duration::from_secs(2);
const TUNNEL_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

pub(crate) struct ClientConfigs {
    pub quic:  quinn::ClientConfig,
    pub roots: rustls::RootCertStore,
}

pub(crate) fn roots() -> anyhow::Result<&'static rustls::RootCertStore> {
    Ok(&core().net.get().ok_or_else(|| anyhow::anyhow!("node dialer not initialized"))?.dialer.roots)
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

/// The outer Control pipe reveals no user identity; the inner protocol keeps its own
/// authentication and privacy boundary.
pub(crate) async fn connect(addr: SocketAddr, name: &str) -> Result<Connection, DialerError> {
    let net = core().net.get().ok_or_else(|| io::Error::other("endpoint not initialized"))?;
    connect_with(&net.endpoint, &net.dialer, addr, name).await
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
                // A simultaneously ready rejection from the other carrier must not hide behind
                // select order. Pending losers are cancelled; successful losers must be closed.
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

/// Owns both transport layers during setup and after handoff: cancelling any await closes the
/// private endpoint and pipe, so late workers never touch a replacement connection.
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
    core().spawn(async move {
        // No Connection clone here: a caller failing its handshake drops its last handle to close
        // it. The endpoint stays live until Quinn finishes that connection's drain.
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
        // Resolved at dial time so a DNS repoint is picked up on reconnect.
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
    use crate::test_support::net;

    fn configs(role: ProtoRole, roots: rustls::RootCertStore) -> ClientConfigs {
        ClientConfigs { quic: net::client_config(role, &roots), roots }
    }

    /// No downgrade: a security failure on either carrier beats a success on the other, and the
    /// successful carrier is closed.
    #[tokio::test]
    async fn a_security_rejection_from_either_carrier_wins() {
        for udp_rejected in [false, true] {
            let (conn, _relay) = net::connection().await;
            let rejected = Err(DialerError::Security(anyhow::anyhow!("rejected relay identity")));
            let (udp, tcp) = if udp_rejected {
                (rejected, Ok(conn.clone()))
            } else {
                (Ok(conn.clone()), rejected)
            };
            let result = race_transports(std::future::ready(udp), std::future::ready(tcp)).await;
            assert!(matches!(result, Err(DialerError::Security(_))), "{result:?}");
            assert!(conn.close_reason().is_some(), "the successful carrier is closed");
        }
    }

    /// A refused certificate or a wrong negotiated protocol over UDP never opens a TCP handshake.
    #[tokio::test]
    async fn a_udp_security_failure_never_tries_tcp() {
        for (role, trusted) in [(ProtoRole::Client, false), (ProtoRole::Peer, true)] {
            let (server, roots) = net::server(role);
            let addr = server.local_addr().unwrap();
            let tcp = std::net::TcpListener::bind(addr).unwrap();
            tcp.set_nonblocking(true).unwrap();
            let serving = tokio::spawn(async move {
                if let Ok(conn) = server.accept().await.unwrap().await {
                    conn.closed().await;
                }
            });
            let roots = if trusted { roots } else { rustls::RootCertStore::empty() };
            let result =
                connect_with(&net::client_endpoint(), &configs(role, roots), addr, "localhost")
                    .await;
            assert!(matches!(result, Err(DialerError::Security(_))), "{role:?}: {result:?}");
            assert_eq!(tcp.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock, "{role:?}");
            serving.await.unwrap();
        }
    }

    /// With UDP blackholed, cold discovery completes over the TLS carrier: both ends derive the
    /// same client-auth binding, the TCP origin is kept, and the relay going away closes promptly.
    #[tokio::test(start_paused = true)]
    async fn udp_blackhole_falls_back_to_anonymous_tls_for_cold_discovery_and_closes_promptly() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use common::proto::client_res::ClientRequest;
        use common::proto::client_res::ClientResponse;
        use common::proto::pack::Packer;
        use common::proto::pack::Unpacker;

        let _clock = net::step_paused_clock();
        let (tls, mut roots) = net::tls_server(tunnel::ALPN);
        let (inner, inner_roots) = net::tls_server(ProtoRole::Client.alpn().as_bytes());
        roots.roots.extend(inner_roots.roots);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Swallows every UDP Initial, so whatever answers cannot have come over native UDP.
        let blackhole = tokio::net::UdpSocket::bind(addr).await.unwrap();
        let swallowed = Arc::new(AtomicUsize::new(0));
        let dropping = tokio::spawn({
            let swallowed = swallowed.clone();
            async move {
                let mut packet = [0; 2048];
                while blackhole.recv_from(&mut packet).await.is_ok() {
                    swallowed.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();
        let (restart_tx, restart_rx) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            let (stream, origin) = listener.accept().await.unwrap();
            let accepted =
                tunnel::accept(stream, Arc::new(tls), tunnel::FEATURE_CONTROL).await.unwrap();
            assert_eq!(accepted.mode, tunnel::AcceptedMode::Control);
            let channel = accepted.channel;
            let socket = channel.clone().socket(channel.local_addr(), channel.peer_addr());
            let endpoint = Endpoint::new_with_abstract_socket(
                quinn::EndpointConfig::default(),
                Some(net::quic_server(inner)),
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap();
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            assert_eq!(conn.remote_address(), origin, "the TCP origin is kept");
            bound_tx.send(common::quic::client_auth_binding(&conn).unwrap()).unwrap();
            let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
            assert!(matches!(
                ClientRequest::unpack(&mut rx).await.unwrap(),
                ClientRequest::GetRelays()
            ));
            tx.write_all(&ClientResponse::GetRelays { relays: vec![] }.pack().unwrap())
                .await
                .unwrap();
            tx.finish().unwrap();
            restart_rx.await.unwrap();
            channel.close();
            endpoint.close(0u32.into(), b"relay restart");
        });

        let conn = connect_with(
            &net::client_endpoint(),
            &configs(ProtoRole::Client, roots),
            addr,
            "localhost",
        )
        .await
        .unwrap();
        assert!(swallowed.load(Ordering::Relaxed) > 0, "UDP was tried first");
        assert_eq!(common::quic::client_auth_binding(&conn).unwrap(), bound_rx.await.unwrap());
        let (mut tx, mut rx) = conn.open_bi().await.unwrap();
        tx.write_all(&ClientRequest::GetRelays().pack().unwrap()).await.unwrap();
        tx.finish().unwrap();
        assert!(matches!(
            ClientResponse::unpack(&mut rx).await.unwrap(),
            ClientResponse::GetRelays { relays } if relays.is_empty()
        ));
        restart_tx.send(()).unwrap();
        // The tunnel guard that closes the connection runs on the core runtime in real time, so a
        // stepped clock could expire this wait before it gets there.
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(2), conn.closed()).await.unwrap();
        serving.await.unwrap();
        dropping.abort();
    }

    /// An old UDP-only relay answering after the TCP head start, with TCP refused, still connects
    /// within the original budget.
    #[tokio::test(start_paused = true)]
    async fn slow_udp_only_node_keeps_its_connect_budget_after_tcp_is_refused() {
        let _clock = net::step_paused_clock();
        let (server, roots) = net::server(ProtoRole::Client);
        let addr = server.local_addr().unwrap();
        drop(std::net::TcpListener::bind(addr).unwrap());
        let serving = tokio::spawn(async move {
            tokio::time::sleep(UDP_HEAD_START + Duration::from_millis(250)).await;
            if let Ok(conn) = server.accept().await.unwrap().await {
                conn.closed().await;
            }
        });
        let started = tokio::time::Instant::now();
        let conn = connect_with(
            &net::client_endpoint(),
            &configs(ProtoRole::Client, roots),
            addr,
            "localhost",
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= UDP_HEAD_START);
        conn.close(0u32.into(), b"done");
        serving.await.unwrap();
    }
}
