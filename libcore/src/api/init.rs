//! Bootstrap: build the QUIC endpoint, install the client-supplied
//! platform ports, and own the relay connection for the process lifetime.
//!
//! Unlike the old JNI `initApi` + `connect` split, the client makes a
//! single `init` call and core sustains the relay link itself — no
//! explicit `connect()`, no online/offline toggle (the OS provides
//! airplane mode; the loop already no-ops on a dead network).

use std::net::UdpSocket;
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
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TransportConfig;
use quinn::default_runtime;

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

    let _guard = RUNTIME.enter();
    let socket = UdpSocket::bind("0.0.0.0:0")?;

    // Client-only endpoint: pairing is async over the DHT now, so there
    // is no incoming peer-to-peer to accept — no server config needed.
    let mut endpoint =
        Endpoint::new(EndpointConfig::default(), None, socket, default_runtime().unwrap())?;

    let roots = load_root_ca_bytes(ROOT_CA)?;
    let mut client_cfg = build_client_cfg(ProtoRole::Client, &roots)?;

    let mut transport_cfg = TransportConfig::default();
    transport_cfg.keep_alive_interval(Some(Duration::from_secs(15)));
    // Bound dead connections/handshakes (keepalive 15s < idle 30s) so a link
    // that goes silent is torn down and the relay loop reconnects.
    transport_cfg
        .max_idle_timeout(Some(Duration::from_secs(30).try_into().expect("valid idle timeout")));
    client_cfg.transport_config(Arc::new(transport_cfg));

    endpoint.set_default_client_config(client_cfg);
    ENDPOINT.set(Arc::new(endpoint)).map_err(|_| anyhow::anyhow!("init called twice"))?;

    start_relay_loop(seeds);
    Ok(())
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

            match Relay::fetch_best() {
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
                        Err(ResolveError::EmptyResponse) => {
                            error!("resolver returned no relays");
                            ConnectionState::Failed.emit();
                            return;
                        },
                        Err(err) => error!("resolver failed: {err}"),
                    }
                    // All known relays may be circuit-open; back off before retry.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                },
                Err(err) => error!("failed to fetch relay: {err}"),
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}
