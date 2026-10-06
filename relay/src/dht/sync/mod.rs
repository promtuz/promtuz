//! DHT maintenance: bootstrap retries, drift migration of queued rows, and bucket refresh.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use common::info;
use common::quic::id::NodeId;
use common::utils::now_ms;
use tokio_util::sync::CancellationToken;

use super::Dht;
use super::bootstrap::BootstrapError;
use super::bootstrap::bootstrap;
use super::config;

/// Interval of the drift-migration sweep.
const EVICT_INTERVAL_MS: u64 = 60_000;

const BUCKET_REFRESH_SCAN_INTERVAL_MS: u64 = 300_000;

const MAX_BUCKET_REFRESH_PER_SCAN: usize = 4;

const BOOTSTRAP_RETRY_THRESHOLD: usize = 8;

const BOOTSTRAP_RETRY_BASE_MS: u64 = 5_000;

const BOOTSTRAP_RETRY_MAX_BACKOFF_MS: u64 = 300_000;

pub(crate) async fn run_scheduler(dht: Arc<Dht>, cancel: CancellationToken) {
    use tokio::time::interval;

    let mut bootstrap_tick = interval(Duration::from_millis(config::ANTI_ENTROPY_INTERVAL_MS));
    let mut drift_tick = interval(Duration::from_millis(EVICT_INTERVAL_MS));
    let mut refresh_tick = interval(Duration::from_millis(BUCKET_REFRESH_SCAN_INTERVAL_MS));
    // An interval's first tick fires at once; skipping it avoids racing the startup bootstrap.
    bootstrap_tick.tick().await;
    drift_tick.tick().await;
    refresh_tick.tick().await;

    let mut bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
    let mut last_bootstrap_attempt_ms: u64 = 0;
    let mut was_sparse = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("DHT scheduler: cancellation observed; exiting");
                return;
            }
            _ = bootstrap_tick.tick() => {
                let known = dht.routing.read().total_known();
                let sparse = known < BOOTSTRAP_RETRY_THRESHOLD;
                if sparse && !was_sparse {
                    crate::dht_log!("DHT routing table sparse ({known} < {BOOTSTRAP_RETRY_THRESHOLD}); retrying bootstrap");
                } else if !sparse && was_sparse {
                    crate::dht_log!("DHT routing table recovered ({known} >= {BOOTSTRAP_RETRY_THRESHOLD})");
                }
                was_sparse = sparse;
                if sparse {
                    let now = now_ms();
                    if now.saturating_sub(last_bootstrap_attempt_ms) >= bootstrap_backoff_ms {
                        last_bootstrap_attempt_ms = now;

                        let handle_opt = dht.resolver.read().clone();
                        match handle_opt {
                            Some(handle) => match bootstrap(dht.clone(), handle).await {
                                Ok(()) => {
                                    crate::dht_log!("DHT bootstrap retry succeeded");
                                    // A table that did not grow means the network is this small.
                                    bootstrap_backoff_ms = if dht.routing.read().total_known() > known {
                                        BOOTSTRAP_RETRY_BASE_MS
                                    } else {
                                        (bootstrap_backoff_ms * 2).min(BOOTSTRAP_RETRY_MAX_BACKOFF_MS)
                                    };
                                    super::push_replication::retry_pending(dht.clone()).await;
                                },
                                // A brand-new network: hold the base backoff so a peer that
                                // joins soon is found quickly.
                                Err(BootstrapError::EmptyRegistry) => {
                                    bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
                                },
                                Err(e) => {
                                    crate::dht_log!("DHT bootstrap retry failed: {e}; backing off");
                                    bootstrap_backoff_ms =
                                        (bootstrap_backoff_ms * 2).min(BOOTSTRAP_RETRY_MAX_BACKOFF_MS);
                                },
                            },
                            None => {
                                bootstrap_backoff_ms =
                                    (bootstrap_backoff_ms * 2).min(BOOTSTRAP_RETRY_MAX_BACKOFF_MS);
                            },
                        }
                    }
                } else {
                    bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
                    super::push_replication::retry_pending(dht.clone()).await;
                }
            }
            _ = drift_tick.tick() => {
                run_drift_migration_sweep(dht.clone()).await;
            }
            _ = refresh_tick.tick() => {
                run_bucket_refresh(dht.clone()).await;
            }
        }
    }
}

/// A bucket untouched for [`config::BUCKET_REFRESH_MS`] is where dead entries pile up, since no
/// traffic proves them dead. The walk's liveness feedback is what evicts them.
async fn run_bucket_refresh(dht: Arc<Dht>) {
    let stale: Vec<usize> = {
        let routing = dht.routing.read();
        routing
            .buckets_needing_refresh(Instant::now())
            .into_iter()
            .take(MAX_BUCKET_REFRESH_PER_SCAN)
            .collect()
    };
    if stale.is_empty() {
        return;
    }

    for idx in stale {
        let Some(target) = super::routing::random_id_in_bucket(&dht.node_id, idx) else {
            continue;
        };
        match super::lookup::lookup_node(dht.clone(), target).await {
            Ok(peers) => {
                dht.routing.write().mark_refreshed(idx);
                crate::dht_log!("DHT bucket {idx} refreshed: {} peer(s)", peers.len());
            },
            // Leave `refresh_at` alone so the next scan retries.
            Err(e) => crate::dht_log!("DHT bucket {idx} refresh failed: {e}"),
        }
    }
}

pub(crate) async fn run_drift_migration_sweep(dht: Arc<Dht>) {
    let candidates = super::store::plan_drift_migrations(&dht, config::MAX_MIGRATE_PER_SWEEP);
    if candidates.is_empty() {
        return;
    }

    info!(
        "DHT scheduler: drift-migration sweep planning {} candidate(s)",
        candidates.len()
    );

    let now = now_ms();
    crate::quic::handler::client::events::bounded_fanout(
        candidates
            .into_iter()
            .map(|(key, dispatch)| migrate_one(dht.clone(), key, dispatch, now))
            .collect(),
        config::MAX_CONCURRENT_MIGRATIONS,
    )
    .await;
}

async fn migrate_one(
    dht: Arc<Dht>, key: crate::storage::MessageKey,
    dispatch: common::proto::client_rel::DispatchP, now_ms: u64,
) {
    match super::forward::forward_to_homes(dht.clone(), dispatch, now_ms).await {
        Ok(summary) if handover_is_durable(&dht, &summary) => {
            // A failed delete only means a duplicate delivery, and the next sweep retries.
            super::store::delete_migrated_entry(&dht, &key);
        },
        Ok(_) => {
            common::warn!(
                "DHT drift migration: handover not durably attested; keeping local copy"
            );
        },
        Err(_) => {},
    }
}

/// This drops the relay's only durable copy, so a success counts only from a peer whose key this
/// relay proved on a live connection: the dial's pin or a signed `DhtHello`.
fn handover_is_durable(dht: &Dht, summary: &super::forward::ForwardSummary) -> bool {
    let conns = dht.peer_conns.read();
    let attested = summary
        .delivered_at
        .iter()
        .chain(summary.stored_at.iter())
        .filter(|id| **id != dht.node_id)
        .filter(|id| conns.get(id).is_some_and(|(_, pk)| NodeId::new(*pk) == **id))
        .count();
    attested >= config::FORWARD_K_MIN
}
