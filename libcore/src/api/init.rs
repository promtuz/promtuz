//! Bootstrap: build the QUIC endpoint, install the client-supplied
//! platform ports, and own the relay connection for the process lifetime.
//!
//! Unlike the old JNI `initApi` + `connect` split, the client makes a
//! single `init` call and core sustains the relay link itself — no
//! explicit `connect()`, no online/offline toggle (the OS provides
//! airplane mode; the loop already no-ops on a dead network).

use std::net::Ipv6Addr;
use std::sync::Arc;
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

/// Root CA for the relay/resolver TLS, baked in at build time. Sourced from
/// the repo's gitignored `.tls/` — the same dev CA store testnet signs with
/// (see `testnet/src/certs.rs`), so it's never committed. Deployments build
/// against their own `.tls/RootCA.pem`.
const ROOT_CA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.tls/RootCA.pem"));

/// One-time initialization. Installs the platform ports, builds the
/// client-only QUIC endpoint, and starts the relay loop. `resolver_seeds`
/// is the bootstrap seed list the client bundles (see its app resources).
#[uniffi::export]
pub fn init(
    secure_store: Arc<dyn SecureStore>,
    events: Arc<dyn CoreEvents>,
    resolver_seeds: String,
) -> Result<(), CoreError> {
    init_inner(secure_store, events, resolver_seeds)?;
    Ok(())
}

fn init_inner(
    secure_store: Arc<dyn SecureStore>,
    events: Arc<dyn CoreEvents>,
    resolver_seeds: String,
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
    // Long idle timeout so a backgrounded (frozen) app keeps its connection
    // across an app switch — QUIC resumes it via connection-ID path migration
    // on the new NAT port, skipping the reconnect handshake entirely. Must
    // match the relay's server idle (both = IDLE_TIMEOUT_SECS).
    transport_cfg.max_idle_timeout(Some(
        Duration::from_secs(common::quic::config::IDLE_TIMEOUT_SECS)
            .try_into()
            .expect("valid idle timeout"),
    ));
    client_cfg.transport_config(Arc::new(transport_cfg));

    endpoint.set_default_client_config(client_cfg);
    ENDPOINT.set(Arc::new(endpoint)).map_err(|_| anyhow::anyhow!("init called twice"))?;

    start_relay_loop(seeds);
    Ok(())
}

/// Woken when the app returns to the foreground. The relay loop races its
/// post-disconnect backoff against this so a reconnect (needed only after a
/// background longer than the idle timeout) fires immediately instead of
/// waiting out the 2 s retry sleep. A no-op when the connection is still live —
/// QUIC path migration already continued it, nothing to reconnect.
static FOREGROUND: Lazy<tokio::sync::Notify> = Lazy::new(tokio::sync::Notify::new);

/// Client hook: call from the platform's app-foreground lifecycle event.
#[uniffi::export]
pub fn on_foreground() {
    FOREGROUND.notify_waiters();
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
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };

            if !crate::utils::has_internet() {
                ConnectionState::NoInternet.emit();
                tokio::time::sleep(Duration::from_secs(5)).await;
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
                    trace!("connecting to relay({id})");
                    match relay.connect(ipk).await {
                        Ok(handle) => match handle.await {
                            Ok(conn_err) => error!("relay({id}) connection closed: {conn_err}"),
                            Err(join_err) => error!("relay({id}) handle join failed: {join_err}"),
                        },
                        Err(RelayConnError::Continue) => {},
                        Err(RelayConnError::Error(err)) => error!("relay({id}) connect error: {err}"),
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
                        Err(ResolveError::EmptyResponse) => error!("resolver returned no relays; retrying"),
                        Err(err) => error!("resolver failed: {err}; retrying"),
                    }
                    // Short backoff: a fresh resolve may have just populated the
                    // table, or all known relays are circuit-open and will reset.
                    // (Was 30s — too slow for a cold-boot fresh install.)
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                },
                Err(err) => error!("failed to fetch relay: {err}"),
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = FOREGROUND.notified() => trace!("foreground: reconnecting now"),
            }
        }
    });
}
