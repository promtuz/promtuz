//! Bootstrap: build the QUIC endpoint, install the platform ports, and own the relay connection
//! for the process lifetime.

use std::net::Ipv6Addr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use common::quic::config::build_client_cfg;
use common::quic::config::load_root_ca_bytes;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use common::utils::now_secs;
use log::debug;
use log::error;
use log::trace;
use quinn::Endpoint;
use quinn::TransportConfig;
use tokio_util::sync::CancellationToken;

use crate::data::ResolverSeed;
use crate::data::parse_seeds;
use crate::data::identity::Identity;
use crate::data::relay::Relay;
use crate::data::relay::RelayError;
use crate::data::relay::ResolveError;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::platform::CoreError;
use crate::platform::CoreEvents;
use crate::platform::SecureStore;
use crate::quic::dialer::ClientConfigs;
use crate::quic::server::RelayConnError;
use crate::state::Net;
use crate::state::core;
use crate::utils::node_short;

/// Root CA for relay and resolver TLS, read at build time from the gitignored `.tls/RootCA.pem`.
const ROOT_CA: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../.tls/RootCA.pem"));

/// `resolver_seeds` is the bootstrap seed list the client bundles.
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

    let core = core();
    core.secure_store.set(secure_store).map_err(|_| anyhow::anyhow!("init called twice"))?;
    core.events.set(events).map_err(|_| anyhow::anyhow!("init called twice"))?;

    let seeds = parse_seeds(&resolver_seeds)?;

    let _guard = core.runtime.enter();

    // The IPv6 wildcard is dual-stack in quinn and reaches IPv4 peers as v4-mapped; a `0.0.0.0`
    // bind would refuse every IPv6 destination.
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

    endpoint.set_default_client_config(client_cfg.clone());
    let dialer = ClientConfigs { quic: client_cfg, roots };
    core.net
        .set(Net { endpoint, dialer, seeds: seeds.clone() })
        .map_err(|_| anyhow::anyhow!("init called twice"))?;

    core.spawn_blocking(crate::transfer::sweep_orphaned_retention);
    core.supervise("outbox", redrive_outbox);
    core.supervise("relay loop", move |cancel| relay_loop(seeds.clone(), cancel));
    core.supervise("push registration", crate::push::maintain_registration);
    Ok(())
}

/// Re-drives the outbox on a timer so retries and the pending-to-failed timeout fire without a
/// reconnect.
async fn redrive_outbox(cancel: CancellationToken) -> Result<()> {
    let mut ticker = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = ticker.tick() => {},
        }
        if let Some(session) = core().session() {
            crate::delivery::reconcile(&session).await;
        }
        crate::transfer::gc(now_secs());
    }
}

#[uniffi::export]
pub fn on_foreground() {
    core().task_removed.store(false, Ordering::SeqCst);
    // `notify_one` retains a permit when the relay loop has not started waiting.
    core().foreground.notify_one();
    crate::push::request_registration();
    probe_relay_liveness(b"foreground liveness timeout");
    apply_pending_network_change();
}

/// The default network or its addresses or routes changed. Leaves foreground state alone and does
/// not undo an OS task removal.
#[uniffi::export]
pub fn on_network_changed() {
    let core = core();
    core.network_change_pending.store(true, Ordering::SeqCst);
    if core.task_removed.load(Ordering::SeqCst) { return; }
    if let Err(e) = Relay::reset_circuits() {
        log::warn!("relay circuits not reset: {e}");
    }
    core.foreground.notify_one();
    probe_relay_liveness(b"network change liveness timeout");
    apply_pending_network_change();
}

fn apply_pending_network_change() {
    if core().network_change_pending.swap(false, Ordering::SeqCst) {
        core().spawn(crate::p2p::network_changed());
    }
}

fn probe_relay_liveness(close_reason: &'static [u8]) {
    // Probe the captured connection only. A delayed probe must never close its
    // replacement, and repeated lifecycle/network callbacks share one probe.
    let connection = core().session().map(|s| s.conn.clone());
    if let Some(connection) = connection {
        core().spawn(async move {
            let Ok(_probe) = core().relay_probe.try_lock() else { return };
            if connection.close_reason().is_some() { return; }
            if !relay_responds(&connection).await {
                connection.close(0u32.into(), close_reason);
                core().foreground.notify_one();
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

/// False once `cancel` fires.
async fn wait_to_retry(cancel: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(delay) => true,
        _ = core().foreground.notified() => {
            trace!("foreground: reconnecting now");
            true
        },
        _ = cancel.cancelled() => false,
    }
}

/// Closing the connection lets the relay mark us offline at once instead of at idle timeout.
#[uniffi::export]
pub fn on_task_removed() {
    crate::transfer::sharing::set_foreground(false);
    core().task_removed.store(true, Ordering::SeqCst);
    if let Some(session) = core().session() {
        session.conn.close(quinn::VarInt::from_u32(0), b"task removed");
    }
}

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

/// Reconnects until cancelled. Single-flight because only `init` starts it, once.
async fn relay_loop(seeds: Vec<ResolverSeed>, cancel: CancellationToken) -> Result<()> {
    loop {
        let Ok(ipk) = Identity::public_key() else {
            if !wait_to_retry(&cancel, Duration::from_secs(2)).await {
                return Ok(());
            }
            continue;
        };

        if core().task_removed.load(Ordering::Relaxed) {
            ConnectionState::Disconnected.emit();
            tokio::select! {
                _ = core().foreground.notified() => {},
                _ = cancel.cancelled() => return Ok(()),
            }
            continue;
        }

        // A user-picked relay preempts the weighted-random pick unless it was forgotten since.
        let selected = match core().take_preferred_relay() {
            Some(id) => Relay::fetch_by_id(&id).or_else(|_| Relay::fetch_best()),
            None => Relay::fetch_best(),
        };

        let relay = match selected {
            Ok(relay) => relay,
            Err(RelayError::NoneAvailable) => {
                debug!("no relay available, resolving");
                match Relay::resolve(&seeds).await {
                    Ok(_) => {},
                    Err(ResolveError::EmptyResponse) => {
                        error!("resolver returned no relays; retrying")
                    },
                    Err(err) => error!("resolver failed: {err}; retrying"),
                }
                // An all-open table still gets probed through the relay
                // whose backoff ends first.
                match Relay::fetch_best().or_else(|_| Relay::fetch_backoff_candidate()) {
                    Ok(relay) => relay,
                    Err(_) => {
                        if !wait_to_retry(&cancel, Duration::from_secs(5)).await {
                            return Ok(());
                        }
                        continue;
                    },
                }
            },
            Err(err) => {
                error!("failed to fetch relay: {err}");
                if !wait_to_retry(&cancel, Duration::from_secs(2)).await {
                    return Ok(());
                }
                continue;
            },
        };
        let short = node_short(&relay.id);
        trace!("connecting to relay {short}");
        match relay.connect(ipk).await {
            Ok(handle) => match handle.await {
                Ok(conn_err) => error!("relay {short} connection closed: {conn_err}"),
                Err(join_err) => error!("relay {short} handle join failed: {join_err}"),
            },
            Err(RelayConnError::Continue) => {},
            Err(RelayConnError::Error(err)) => error!("relay {short} connect error: {err}"),
        }

        if !wait_to_retry(&cancel, Duration::from_secs(2)).await {
            return Ok(());
        }
    }
}
