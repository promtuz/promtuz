//! Optional TLS/TCP listener for phones. A control pipe carries exactly one inner QUIC client
//! connection; no pipe can act as a UDP proxy or register a node.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use quinn::{Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime};
use tokio::net::TcpListener;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use super::config::build_server_cfg;
use super::protorole::ProtoRole;
use super::tunnel::{self, AcceptedMode, Channel};
use crate::node::config::NetworkConfig;
use crate::server::accept::{Gate, Policy, SWEEP_INTERVAL, drain};

const ACCEPT: Policy =
    Policy { per_minute: 120, burst: 60, max_live: 1024, max_live_per_source: 256 };
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
        // Client role only; node roles stay on the native UDP endpoint.
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
        let mut gate = Gate::new(&ACCEPT);
        let mut tasks = JoinSet::new();
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = sweep.tick() => gate.sweep(),
                _ = tasks.join_next(), if !tasks.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, source) = match accepted {
                        Ok(pair) => pair,
                        Err(error) => {
                            crate::warn!("TLS fallback accept failed: {error}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            continue;
                        },
                    };
                    let Ok(permit) = gate.admit(source.ip()) else { continue; };
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
                                    crate::debug!("TLS fallback control ended: {error}");
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
        drain(tasks).await;
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
