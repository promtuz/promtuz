//! Maintains connection with resolver
//!
//! ## Sharing the live session for ad-hoc RPCs
//!
//! `ResolverLink` is a fire-and-forget background task: `attach()`
//! consumes `self`, leaving callers with only a `JoinHandle`. That works
//! for the original lifecycle traffic (`RelayHello` / heartbeats) which
//! the link drives autonomously, but not for code that needs to *send a
//! one-shot request* over the same connection — most notably the DHT
//! bootstrap path.
//!
//! [`ResolverLinkHandle`] solves this with a small shared state shim:
//! the link writes the current `Connection` into a `RwLock<Option<...>>`
//! at session start, clears it at session end, and any task holding a
//! cloned handle can take a snapshot of that connection (cheap `Arc`
//! clone behind quinn's `Connection`) without going through the link
//! task. The handle is given out at `ResolverLink::new` time and
//! survives the move into `attach()` because it's just an `Arc`.
//!
//! Locks are `parking_lot` and held only across the snapshot — never
//! across an `await` — matching the project-wide convention also used
//! by `Dht::peer_conns` (`relay/src/dht/mod.rs`).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use common::debug;
use common::info;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::GatewayDescriptor;
use common::proto::client_res::RelayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::proto::relay_res::relay_heartbeat_signing_input;
use common::proto::relay_res::relay_hello_signing_input;
use common::quic::CloseReason;
use common::quic::RESOLVER_RELAY_HEARTBEAT_INTERVAL;
use common::quic::id::NodeId;
use common::sysutils::system_load;
use common::types::bytes::Bytes;
use common::warn;
use ed25519_dalek::Signer;
use parking_lot::RwLock;
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

/// Cloneable handle to the live resolver session, used by code outside
/// the link task that wants to send one-shot requests over the same
/// connection (currently: DHT bootstrap).
///
/// The link writes the active `Connection` in at session start and
/// clears it at session end. Holders observe `None` while the link is
/// reconnecting; callers handle that by erroring out (bootstrap retries
/// are scheduled separately).
///
/// Cheap to clone: just the `Arc<RwLock<Option<Connection>>>` inside.
#[derive(Clone, Debug)]
pub struct ResolverLinkHandle {
    /// Current resolver connection, if a session is live. Written by
    /// `ResolverLink::run_session` on connect/disconnect. The
    /// `Connection` itself is internally `Arc`-shared by quinn so a
    /// clone here doesn't duplicate any underlying state.
    inner: Arc<RwLock<Option<Connection>>>,
}

impl ResolverLinkHandle {
    fn empty() -> Self {
        Self { inner: Arc::new(RwLock::new(None)) }
    }

    /// Snapshot the current resolver `Connection` if a session is live.
    /// The lock is dropped before the caller does any I/O, satisfying
    /// the project-wide "no parking_lot guards across await" rule.
    fn current_connection(&self) -> Option<Connection> {
        self.inner.read().clone()
    }

    /// Send a [`ClientRequest::GetBootstrapPeers`] over the live
    /// resolver session and decode the matching
    /// [`ClientResponse::GetBootstrapPeers`].
    ///
    /// Errors:
    /// - [`anyhow::Error`] wrapping "no live resolver session" if the
    ///   link is currently reconnecting.
    /// - QUIC stream errors propagate up (open_bi / write / read).
    /// - A response with a non-matching variant returns a decode-style
    ///   error so the caller doesn't silently misinterpret a
    ///   `GetRelays`-shaped reply that was queued behind some races.
    pub async fn get_bootstrap_peers(
        &self, near: [u8; 32], count_xor_near: u8, count_rtt_near: u8,
    ) -> Result<(Vec<RelayDescriptor>, Vec<RelayDescriptor>)> {
        let conn = self
            .current_connection()
            .context("no live resolver session for GetBootstrapPeers")?;

        let req = ClientRequest::GetBootstrapPeers { near, count_xor_near, count_rtt_near };
        let bytes = req.pack()?;

        // One bi-stream per RPC, mirroring the resolver-side
        // `handle_client` contract documented at
        // `resolver/src/quic/handler/client.rs:14-20`.
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&bytes).await?;
        send.finish()?;

        let resp = ClientResponse::unpack(&mut recv).await?;
        match resp {
            ClientResponse::GetBootstrapPeers { xor_near, rtt_near } => Ok((xor_near, rtt_near)),
            other => Err(anyhow!(
                "GetBootstrapPeers: resolver returned unexpected variant {:?}",
                other
            )),
        }
    }

    /// Fetch the resolver's push-gateway directory. One bi-stream per RPC, same
    /// contract as [`Self::get_bootstrap_peers`].
    pub async fn get_gateways(&self) -> Result<Vec<GatewayDescriptor>> {
        let conn =
            self.current_connection().context("no live resolver session for GetGateways")?;

        let bytes = ClientRequest::GetGateways().pack()?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&bytes).await?;
        send.finish()?;

        match ClientResponse::unpack(&mut recv).await? {
            ClientResponse::GetGateways { gateways } => Ok(gateways),
            other => Err(anyhow!("GetGateways: resolver returned unexpected variant {:?}", other)),
        }
    }
}

pub struct ResolverLink {
    relay: Arc<Relay>,
    shutdown: Receiver<()>,
    cfg: ClientConfig,
    backoff: BackoffConfig,
    /// Shared handle: stays in sync with the current session for any
    /// outside caller (e.g. DHT bootstrap) that needs to send a one-shot
    /// RPC.
    handle: ResolverLinkHandle,
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

        Self {
            relay,
            shutdown: rx,
            cfg,
            backoff: BackoffConfig::default(),
            handle: ResolverLinkHandle::empty(),
        }
    }

    /// Cheap-to-clone view of the live resolver session for ad-hoc RPCs.
    /// Hand the returned handle to anything that wants to send requests
    /// alongside the lifecycle traffic (currently: DHT bootstrap).
    ///
    /// Named `client_handle` (not `handle`) to keep the inbound-stream
    /// dispatcher `Self::handle` collision-free — the dispatcher is
    /// already the canonical "handle the session" method on this type.
    pub fn client_handle(&self) -> ResolverLinkHandle {
        self.handle.clone()
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

        // Publish the live connection so handle holders (DHT bootstrap)
        // can issue ad-hoc RPCs over the same session. Cleared in the
        // `defer`-style guard below — even on early exit from
        // `handle()` — so a stale handle never points at a dead conn.
        *self.handle.inner.write() = Some(conn.clone());

        // Spawn the periodic heartbeat alongside the inbound handler. Both
        // share the same `Connection`; whichever ends first cancels the
        // other via the abort handle.
        let heartbeat = tokio::spawn(Self::heartbeat_loop(
            conn.clone(),
            self.relay.clone(),
            self.shutdown.clone(),
        ));

        let res = self.handle(&conn).await;
        heartbeat.abort();

        // Session over: clear the shared handle so any subsequent
        // `get_bootstrap_peers` call surfaces "no live session" instead
        // of trying to open a stream on a closed connection.
        *self.handle.inner.write() = None;

        res
    }

    async fn hello(&self, conn: &Connection) -> Result<()> {
        let mut send = conn.open_uni().await?;

        let relay_id = self.id();
        let pubkey = self.relay.keys.public.to_bytes();
        let timestamp = systime().as_millis();

        // Sign the canonical transcript so the resolver can authenticate this
        // relay before admitting it to the registry.
        let msg = relay_hello_signing_input(&relay_id, &pubkey, timestamp);
        let sig = self.relay.keys.signing.sign(&msg).to_bytes();

        debug!("sending to resolver({})", conn.remote_address());
        ResolverPacket::Lifetime(LifetimeP::RelayHello {
            relay_id,
            pubkey: Bytes(pubkey),
            timestamp,
            sig: Bytes(sig),
        })
        .send(&mut send)
        .await?;

        send.finish()?;

        Ok(())
    }

    /// Sends a periodic, signed [`LifetimeP::RelayHeartbeat`] over a fresh
    /// uni-stream every [`RESOLVER_RELAY_HEARTBEAT_INTERVAL`] seconds.
    ///
    /// Each heartbeat is independently authenticated (full pubkey + Ed25519
    /// signature + fresh timestamp) so the resolver can verify liveness
    /// without trusting the connection alone — this matters for any future
    /// liveness / load-aware routing logic that consumes these packets.
    async fn heartbeat_loop(
        conn: Connection, relay: Arc<Relay>, mut shutdown: Receiver<()>,
    ) {
        let mut tick =
            tokio::time::interval(Duration::from_secs(RESOLVER_RELAY_HEARTBEAT_INTERVAL));
        // Skip the immediate first tick — we just sent `RelayHello`, the
        // resolver doesn't need a heartbeat in the same instant.
        tick.tick().await;

        let start = std::time::Instant::now();

        loop {
            tokio::select! {
                _ = tick.tick() => {},
                _ = shutdown.changed() => return,
            }

            if let Err(e) = Self::send_heartbeat(&conn, &relay, start).await {
                warn!(
                    "heartbeat to resolver({}) failed: {e}",
                    conn.remote_address()
                );
                return;
            }
        }
    }

    async fn send_heartbeat(
        conn: &Connection, relay: &Relay, start: std::time::Instant,
    ) -> Result<()> {
        let relay_id = relay.key.id();
        let pubkey = relay.keys.public.to_bytes();
        let timestamp = systime().as_millis();

        let msg = relay_heartbeat_signing_input(&relay_id, &pubkey, timestamp);
        let sig = relay.keys.signing.sign(&msg).to_bytes();

        let load = system_load().await;
        let uptime_seconds = start.elapsed().as_secs();

        let mut send = conn.open_uni().await?;
        ResolverPacket::Lifetime(LifetimeP::RelayHeartbeat {
            relay_id,
            pubkey: Bytes(pubkey),
            timestamp,
            sig: Bytes(sig),
            load,
            uptime_seconds,
        })
        .send(&mut send)
        .await?;

        send.finish()?;
        debug!("heartbeat -> resolver({})", conn.remote_address());
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
