//! Bootstrap: build the QUIC endpoint, install the client-supplied
//! platform ports, and own the relay connection for the process lifetime.
//!
//! Unlike the old JNI `initApi` + `connect` split, the client makes a
//! single `init` call and core sustains the relay link itself — no
//! explicit `connect()`, no online/offline toggle (the OS provides
//! airplane mode; the loop already no-ops on a dead network).

use std::net::Ipv6Addr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca_bytes;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use log::debug;
use log::error;
use log::trace;
use once_cell::sync::Lazy;
use quinn::Endpoint;
use quinn::TransportConfig;

use crate::ENDPOINT;
use crate::RUNTIME;
use crate::data::ResolverSeed;
use crate::data::ResolverSeeds;
use crate::data::identity::Identity;
use crate::data::relay::Relay;
use crate::data::relay::RelayError;
use crate::data::relay::ResolveError;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::platform::CoreError;
use crate::platform::CoreEvents;
use crate::platform::EVENTS;
use crate::platform::SECURE_STORE;
use crate::platform::SecureStore;
use crate::quic::server::RelayConnError;
use crate::utils::node_short;

/// Root CA for the relay/resolver TLS, baked in at build time. Sourced from
/// the repo's gitignored `.tls/` — the dev CA store `certgen init` mints, so
/// it's never committed. Deployments build against their own
/// `.tls/RootCA.pem`.
const ROOT_CA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.tls/RootCA.pem"));

/// One-time initialization. Installs the platform ports, builds the
/// client-only QUIC endpoint, and starts the relay loop. `resolver_seeds`
/// is the bootstrap seed list the client bundles (see its app resources).
#[uniffi::export]
pub fn init(
    secure_store: Arc<dyn SecureStore>, events: Arc<dyn CoreEvents>, resolver_seeds: String,
) -> Result<(), CoreError> {
    init_inner(secure_store, events, resolver_seeds)?;
    Ok(())
}

fn init_inner(
    secure_store: Arc<dyn SecureStore>, events: Arc<dyn CoreEvents>, resolver_seeds: String,
) -> Result<()> {
    init_logging();
    setup_crypto_provider()?;

    SECURE_STORE.set(secure_store).map_err(|_| anyhow::anyhow!("init called twice"))?;
    EVENTS.set(events).map_err(|_| anyhow::anyhow!("init called twice"))?;

    let seeds = ResolverSeeds::from_str(&resolver_seeds)?;
    crate::RESOLVER_SEEDS.set(seeds.clone()).ok();

    let _guard = RUNTIME.enter();

    // Client-only endpoint (pairing is async over the DHT, so nothing to
    // accept — no server config). Bound to the IPv6 wildcard, which quinn
    // makes dual-stack: it dials both IPv6 *and* IPv4 relays/resolvers
    // (v4 destinations are sent v4-mapped). A `0.0.0.0` bind would refuse
    // any IPv6 destination — the reason libcore couldn't reach an
    // IPv6-resolving relay/resolver.
    let mut endpoint = Endpoint::client((Ipv6Addr::UNSPECIFIED, 0).into())?;

    let roots = load_root_ca_bytes(ROOT_CA)?;
    let mut client_cfg = build_client_cfg(ProtoRole::Client, &roots)?;

    let mut transport_cfg = TransportConfig::default();
    transport_cfg.keep_alive_interval(Some(Duration::from_secs(15)));
    // Must match the relay's server idle; foreground keepalives keep the link
    // open, while a frozen background app ages out quickly.
    transport_cfg.max_idle_timeout(Some(
        Duration::from_secs(common::quic::config::IDLE_TIMEOUT_SECS)
            .try_into()
            .expect("valid idle timeout"),
    ));
    client_cfg.transport_config(Arc::new(transport_cfg));

    endpoint.set_default_client_config(client_cfg);
    ENDPOINT.set(Arc::new(endpoint)).map_err(|_| anyhow::anyhow!("init called twice"))?;

    RUNTIME.spawn_blocking(crate::transfer::sweep_orphaned_retention);

    // Re-drive the outbox on a timer so retries + the pending→failed timeout fire without a
    // reconnect.
    RUNTIME.spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            crate::delivery::reconcile().await;
            crate::RUNTIME.spawn(crate::contact_requests::retry_outgoing());
            crate::transfer::gc(crate::utils::systime().as_secs());
        }
    });

    start_relay_loop(seeds);
    RUNTIME.spawn(crate::push::maintain_registration());
    Ok(())
}

/// Woken when the app returns to the foreground. The relay loop races its
/// post-disconnect backoff against this so a reconnect fires immediately instead
/// of waiting out the 2 s retry sleep.
static FOREGROUND: Lazy<tokio::sync::Notify> = Lazy::new(tokio::sync::Notify::new);
static FOREGROUND_PROBE: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
static TASK_REMOVED: AtomicBool = AtomicBool::new(false);

/// Client hook: call from the platform's app-foreground lifecycle event.
#[uniffi::export]
pub fn on_foreground() {
    TASK_REMOVED.store(false, Ordering::Relaxed);
    // `notify_one` retains a permit when the relay loop has not started waiting.
    FOREGROUND.notify_one();
    crate::push::request_registration();
    // Probe the captured connection only. A delayed probe must never close its
    // replacement, and repeated lifecycle/network callbacks share one probe.
    let connection = crate::state::RELAY.read().as_ref().and_then(|r| r.connection.clone());
    if let Some(connection) = connection {
        RUNTIME.spawn(async move {
            let Ok(_probe) = FOREGROUND_PROBE.try_lock() else { return };
            if connection.close_reason().is_some() { return; }
            if !relay_responds(&connection).await {
                connection.close(0u32.into(), b"foreground liveness timeout");
                FOREGROUND.notify_one();
            }
        });
    }
}

async fn relay_responds(connection: &quinn::Connection) -> bool {
    use common::proto::client_rel::{CRelayPacket, QueryP, QueryResultP, SRelayPacket};
    use common::proto::Sender;
    use common::proto::pack::Unpacker;
    bounded_probe(async {
        let (mut tx, mut rx) = connection.open_bi().await?;
        CRelayPacket::Query(QueryP::PubAddress).send(&mut tx).await?;
        tx.finish()?;
        anyhow::ensure!(matches!(SRelayPacket::unpack(&mut rx).await?,
            SRelayPacket::QueryResult(QueryResultP::PubAddress { .. })), "unexpected liveness reply");
        Ok(())
    }).await
}

async fn bounded_probe(probe: impl std::future::Future<Output = anyhow::Result<()>>) -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(3), probe).await, Ok(Ok(())))
}

async fn wait_to_retry(delay: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = FOREGROUND.notified() => trace!("foreground: reconnecting now"),
    }
}

/// Client hook: call when the OS task is removed. Best-effort close makes the
/// relay mark us offline immediately instead of waiting for idle timeout.
#[uniffi::export]
pub fn on_task_removed() {
    TASK_REMOVED.store(true, Ordering::Relaxed);
    if let Some(relay) = crate::state::RELAY.read().as_ref() {
        if let Some(conn) = &relay.connection {
            conn.close(quinn::VarInt::from_u32(0), b"task removed");
        }
    }
}

/// Initialize logging. ponytail: android_logger writes to logcat on
/// Android and no-ops elsewhere; per-platform logging (oslog on iOS,
/// env_logger on desktop) is future work when those clients land.
fn init_logging() {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_tag("core")
            .with_filter(
                android_logger::FilterBuilder::new()
                    .filter(None, log::LevelFilter::Off)
                    .filter_module("core", log::LevelFilter::Debug)
                    .build(),
            ),
    );
}

/// Core-owned relay connection. Reconnects forever; waits for an identity
/// to exist (enrollment may not have happened yet) and backs off when the
/// network is down or the relay set needs re-resolving. Single-flight by
/// construction — only `init` spawns it, once.
fn start_relay_loop(seeds: Vec<ResolverSeed>) {
    RUNTIME.spawn(async move {
        loop {
            // No identity yet (pre-enrollment): idle until there is one.
            let Ok(ipk) = Identity::public_key() else {
                wait_to_retry(Duration::from_secs(2)).await;
                continue;
            };

            if TASK_REMOVED.load(Ordering::Relaxed) {
                ConnectionState::Disconnected.emit();
                FOREGROUND.notified().await;
                continue;
            }

            if !crate::utils::has_internet() {
                ConnectionState::NoInternet.emit();
                wait_to_retry(Duration::from_secs(5)).await;
                continue;
            }

            // A user-picked relay (Connect/Reconnect action) preempts the
            // weighted-random pick for this iteration; fall back to normal
            // selection if it was forgotten in the meantime.
            let selected = match crate::state::take_preferred_relay() {
                Some(id) => Relay::fetch_by_id(&id).or_else(|_| Relay::fetch_best()),
                None => Relay::fetch_best(),
            };

            match selected {
                Ok(relay) => {
                    let id = relay.id.clone();
                    let short = node_short(&id);
                    trace!("connecting to relay {short}");
                    match relay.connect(ipk).await {
                        Ok(handle) => match handle.await {
                            Ok(conn_err) => error!("relay {short} connection closed: {conn_err}"),
                            Err(join_err) => error!("relay {short} handle join failed: {join_err}"),
                        },
                        Err(RelayConnError::Continue) => {},
                        Err(RelayConnError::Error(err)) => {
                            error!("relay {short} connect error: {err}")
                        },
                    }
                },
                Err(RelayError::NoneAvailable) => {
                    debug!("no relays in database, resolving");
                    match Relay::resolve(&seeds).await {
                        Ok(_) => {},
                        // NEVER return here. `return` exits the one and only
                        // relay-loop task — an empty/failed resolve (common on a
                        // fresh install when the network is still warming up)
                        // then permanently bricked the app: all relays greyed,
                        // no relay ever retried, unrecoverable without a restart.
                        // Log and fall through to a short backoff + retry.
                        Err(ResolveError::EmptyResponse) => {
                            error!("resolver returned no relays; retrying")
                        },
                        Err(err) => error!("resolver failed: {err}; retrying"),
                    }
                    // Short backoff: a fresh resolve may have just populated the
                    // table, or all known relays are circuit-open and will reset.
                    // (Was 30s — too slow for a cold-boot fresh install.)
                    wait_to_retry(Duration::from_secs(5)).await;
                    continue;
                },
                Err(err) => error!("failed to fetch relay: {err}"),
            }

            wait_to_retry(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod foreground_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn stale_probe_is_bounded_but_healthy_and_failed_probes_finish_immediately() {
        let start = tokio::time::Instant::now();
        assert!(bounded_probe(async { Ok(()) }).await);
        assert!(!bounded_probe(async { anyhow::bail!("closed") }).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert!(!bounded_probe(std::future::pending()).await);
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn foreground_wakes_all_retry_delays() {
        for seconds in [2, 5] {
            let start = tokio::time::Instant::now();
            FOREGROUND.notify_one();
            wait_to_retry(Duration::from_secs(seconds)).await;
            assert_eq!(start.elapsed(), Duration::ZERO);
        }
    }
}
