//! Maintains connection with resolver

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::debug;
use common::info;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::quic::CloseReason;
use common::quic::id::NodeId;
use common::warn;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::TransportConfig;
use tokio::sync::watch::Receiver;
use tokio::task::JoinHandle;

use crate::quic::dialer::connect_to_any_seed;
use crate::relay::Relay;
use crate::util::systime;

/// Exponential backoff configuration for resolver reconnection attempts.
struct BackoffConfig {
    initial: Duration,
    max: Duration,
    multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self { initial: Duration::from_secs(1), max: Duration::from_secs(60), multiplier: 2.0 }
    }
}

impl BackoffConfig {
    fn next(&self, current: Duration) -> Duration {
        let next = current.mul_f64(self.multiplier);
        next.min(self.max)
    }
}

pub struct ResolverLink {
    relay: Arc<Relay>,
    shutdown: Receiver<()>,
    cfg: ClientConfig,
    backoff: BackoffConfig,
}

impl ResolverLink {
    /// Transport config for `Relay <-> Resolver`
    fn transport_cfg() -> Arc<TransportConfig> {
        let mut cfg = TransportConfig::default();
        cfg.keep_alive_interval(Some(Duration::from_secs(15)));

        Arc::new(cfg)
    }

    fn id(&self) -> NodeId {
        self.relay.key.id()
    }

    pub fn new(relay: Arc<Relay>, rx: Receiver<()>) -> Self {
        let mut cfg = (*relay.client_cfg).clone();
        cfg.transport_config(Self::transport_cfg());

        Self { relay, shutdown: rx, cfg, backoff: BackoffConfig::default() }
    }

    /// Spawns the resolver link loop. Best-effort: never blocks the caller,
    /// retries with exponential backoff on failure.
    pub fn attach(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut delay = self.backoff.initial;

            loop {
                if self.shutdown.has_changed().unwrap_or(true) {
                    break;
                }

                match self.try_connect_and_run(&mut delay).await {
                    // Shutdown was requested cleanly during the session.
                    Err(e) if is_shutdown(&e) => break,
                    Ok(()) => {
                        // Session ended without error (e.g. resolver closed cleanly).
                        // Reset backoff, reconnect immediately.
                        delay = self.backoff.initial;
                    },
                    Err(e) => {
                        warn!("resolver session ended with error: {e}; retrying in {:?}", delay);
                    },
                }

                // Wait for backoff duration, but respect shutdown during the wait.
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {},
                    _ = self.shutdown.changed() => break,
                }

                delay = self.backoff.next(delay);
            }

            info!("resolver link shutting down");
        })
    }

    /// Attempts a single connection and runs the session until it ends.
    async fn try_connect_and_run(&mut self, delay: &mut Duration) -> Result<()> {
        let conn = tokio::select! {
            result = connect_to_any_seed(
                &self.relay.endpoint,
                &self.relay.cfg.resolver.seed,
                Some(self.cfg.clone()),
            ) => result?,
            _ = self.shutdown.changed() => return Err(ShutdownError.into()),
        };

        info!("resolver session started: {}", conn.remote_address());

        *delay = self.backoff.initial;

        self.run_session(conn).await
    }

    /// Drives an active resolver session.
    async fn run_session(&mut self, conn: Connection) -> Result<()> {
        self.hello(&conn)
            .await
            .inspect_err(|e| warn!("hello to resolver({}) failed: {e}", conn.remote_address()))?;

        self.handle(&conn).await?;

        Ok(())
    }

    async fn hello(&self, conn: &Connection) -> Result<()> {
        let mut send = conn.open_uni().await?;

        debug!("sending to resolver({})", conn.remote_address());
        ResolverPacket::Lifetime(LifetimeP::RelayHello {
            relay_id: self.id(),
            timestamp: systime().as_millis(),
        })
        .send(&mut send)
        .await?;

        send.finish()?;

        Ok(())
    }

    async fn handle(&mut self, conn: &Connection) -> Result<()> {
        loop {
            let mut recv = tokio::select! {
                _ = self.shutdown.changed() => {
                    conn.close(CloseReason::ShuttingDown.code(), b"RelayShuttingDown");
                    return Err(ShutdownError.into());
                },
                res = conn.accept_uni() => res?,
            };

            use LifetimeP::*;
            use ResolverPacket::*;
            match ResolverPacket::unpack(&mut recv).await? {
                Lifetime(HelloAck { resolver_time, .. }) => {
                    debug!(
                        "acknowledged by resolver({}) at {}",
                        conn.remote_address(),
                        resolver_time
                    );
                },
                packet => {
                    debug!("recv packet {:?}", packet);
                },
            }
        }
    }
}

/// Sentinel error used to signal intentional shutdown through the `Result` path.
#[derive(Debug, thiserror::Error)]
#[error("shutdown requested")]
struct ShutdownError;

fn is_shutdown(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ShutdownError>().is_some()
}
