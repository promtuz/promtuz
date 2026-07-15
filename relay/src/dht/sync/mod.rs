//! DHT maintenance scheduler.
//!
//! One `tokio::spawn` task, driven by the relay's `CancellationToken`,
//! runs two independent cadences:
//!
//! 1. **Bootstrap retry** every [`config::ANTI_ENTROPY_INTERVAL_MS`]:
//!    when the routing table is sparse (fewer than
//!    [`BOOTSTRAP_RETRY_THRESHOLD`] known peers) re-ask the resolver,
//!    with exponential backoff so a long-down resolver doesn't turn the
//!    relay into a CPU-soak.
//!
//! 2. **K-set drift migration** every [`EVICT_INTERVAL_MS`]: sweep
//!    `cf_dht_queue` for entries whose recipient is no longer in this
//!    relay's K-closest set and `Forward` them to the new homes.
//!
//! Cancellation: every `select!` arm includes `cancel.cancelled().await`;
//! the loop exits cleanly within one cadence-tick of the token firing.

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::info;
use tokio_util::sync::CancellationToken;

use super::Dht;
use super::bootstrap::BootstrapError;
use super::bootstrap::bootstrap;
use super::config;

// ---------------------------------------------------------------------------
// Scheduler tunables
// ---------------------------------------------------------------------------

/// How often the drift-migration sweep runs over `cf_dht_queue`.
const EVICT_INTERVAL_MS: u64 = 60_000;

/// Routing-table size below which we re-trigger bootstrap.
///
/// Fewer than 8 known peers means we may be operating on a near-empty
/// routing table and any lookup will likely fail.
const BOOTSTRAP_RETRY_THRESHOLD: usize = 8;

/// Initial bootstrap-retry backoff. Doubles up to
/// [`BOOTSTRAP_RETRY_MAX_BACKOFF_MS`].
const BOOTSTRAP_RETRY_BASE_MS: u64 = 5_000;

/// Cap on the bootstrap-retry backoff — 5 minutes.
const BOOTSTRAP_RETRY_MAX_BACKOFF_MS: u64 = 300_000;

/// Wall-clock now in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Maintenance scheduler. Spawns nothing of its own — the caller is
/// `tokio::spawn(run_scheduler(dht, cancel))`.
///
/// On `cancel.cancelled().await` the loop exits cleanly within one
/// cadence-tick. Every arm logs and continues so a transient failure
/// (peer down, network blip, fjall error) doesn't kill the scheduler.
pub(crate) async fn run_scheduler(dht: Arc<Dht>, cancel: CancellationToken) {
    use tokio::time::interval;

    let mut bootstrap_tick = interval(Duration::from_millis(config::ANTI_ENTROPY_INTERVAL_MS));
    let mut drift_tick = interval(Duration::from_millis(EVICT_INTERVAL_MS));
    // `tokio::time::interval` fires once at construction by default;
    // skip that immediate fire so we don't race the bootstrap path.
    bootstrap_tick.tick().await;
    drift_tick.tick().await;

    // Bootstrap-retry state:
    // - `bootstrap_backoff_ms` doubles after each failed retry, capped
    //   at `BOOTSTRAP_RETRY_MAX_BACKOFF_MS`.
    // - `last_bootstrap_attempt_ms` is the wall-clock of the last
    //   *attempt* — we only retry when the backoff window has elapsed.
    let mut bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
    let mut last_bootstrap_attempt_ms: u64 = 0;
    // Edge-trigger the sparse-table notice: log on entering/leaving the
    // sparse state, not on every tick.
    let mut was_sparse = false;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("DHT scheduler: cancellation observed; exiting");
                return;
            }
            _ = bootstrap_tick.tick() => {
                // Bootstrap-retry: when the routing table is sparse,
                // re-ask the resolver. The handle is `Option`-wrapped
                // on `Dht` so unit-test fixtures (no resolver link)
                // skip this branch silently. See `Dht::attach_resolver`.
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

                        // Snapshot the resolver handle out of the RwLock so the
                        // lock isn't held across the `await`.
                        let handle_opt = dht.resolver.read().clone();
                        match handle_opt {
                            Some(handle) => match bootstrap(dht.clone(), handle).await {
                                Ok(state) => {
                                    crate::dht_log!("DHT bootstrap retry succeeded (state {state:?})");
                                    bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
                                    super::push_replication::retry_pending(dht.clone()).await;
                                },
                                // Brand-new-network case — hold base backoff so a
                                // peer joining shortly after lets us re-converge.
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
                // Lazy K-set drift migration: sweep `cf_dht_queue` for
                // entries whose recipient is no longer in this relay's
                // K-closest set and migrate them to the new K-closest.
                // Bounded by `MAX_MIGRATE_PER_SWEEP` (256 candidates)
                // and `MAX_CONCURRENT_MIGRATIONS` (8 in-flight tasks).
                run_drift_migration_sweep(dht.clone()).await;
            }
        }
    }
}

/// Drive one drift-migration sweep: walk `cf_dht_queue` for candidates
/// whose recipient is no longer in this relay's K-closest set, fan out
/// `Forward` RPCs to the new homes, and on success delete the local
/// entry.
///
/// **Why split out from `run_scheduler`**: keeps the scheduler's
/// `select!` block readable AND lets unit tests drive the sweep
/// without spinning up the full interval loop.
///
/// **Bounded fan-out**:
/// - `MAX_MIGRATE_PER_SWEEP` (256) candidates per sweep.
/// - [`config::MAX_CONCURRENT_MIGRATIONS`] (8) in-flight migration tasks.
pub(crate) async fn run_drift_migration_sweep(dht: Arc<Dht>) {
    use tokio::task::JoinSet;

    use super::config::MAX_MIGRATE_PER_SWEEP;

    // 1. Plan: snapshot the candidate list out of the synchronous
    //    planner (which holds the routing-table lock briefly per
    //    cached recipient — see `plan_drift_migrations`).
    let candidates = super::store::plan_drift_migrations(&dht, MAX_MIGRATE_PER_SWEEP);
    if candidates.is_empty() {
        return;
    }

    info!(
        "DHT scheduler: drift-migration sweep planning {} candidate(s)",
        candidates.len()
    );

    // 2. Drive: bounded-concurrency JoinSet, draining tasks as they
    //    complete and submitting from `iter` until empty.
    let mut iter = candidates.into_iter();
    let mut set: JoinSet<()> = JoinSet::new();
    let now = now_ms();

    // Prime the set up to MAX_CONCURRENT_MIGRATIONS slots.
    for _ in 0..config::MAX_CONCURRENT_MIGRATIONS {
        let Some((key, dispatch)) = iter.next() else {
            break;
        };
        let dht_clone = dht.clone();
        set.spawn(async move {
            migrate_one(dht_clone, key, dispatch, now).await;
        });
    }

    // Refill on every completion until iter is exhausted.
    while !set.is_empty() {
        let _ = set.join_next().await;
        if let Some((key, dispatch)) = iter.next() {
            let dht_clone = dht.clone();
            set.spawn(async move {
                migrate_one(dht_clone, key, dispatch, now).await;
            });
        }
    }
}

/// Single-candidate migration step: run the sender-side
/// `forward_to_homes` fan-out for `dispatch`, and on success
/// (≥`FORWARD_K_MIN` homes Stored / Delivered) delete the local
/// `cf_dht_queue` entry. On failure the local entry is preserved for
/// the next sweep.
async fn migrate_one(
    dht: Arc<Dht>, key: crate::storage::MessageKey,
    dispatch: common::proto::client_rel::DispatchP, now_ms: u64,
) {
    dht.metrics.inc_migrations_attempted();
    match super::forward::forward_to_homes(dht.clone(), dispatch, now_ms).await {
        Ok(_summary) => {
            // Success → delete the local entry. Best-effort; a fjall
            // write error re-tries next sweep, but the message is
            // already durably stored at the new K-closest, so duplicate
            // delivery (the only failure mode) is benign.
            super::store::delete_migrated_entry(&dht, &key);
            dht.metrics.inc_migrations_succeeded();
        }
        Err(_e) => {
            dht.metrics.inc_migrations_failed();
            // Don't delete; next sweep tries again.
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::quic::id::NodeId;
    use ed25519_dalek::SigningKey;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;

    /// Module-local test fixture: a fresh on-disk `Dht` at a unique temp
    /// path, `self_id` seeded from `self_seed0`.
    fn fresh_dht(tag: &str, self_seed0: u8) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-{tag}-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let mut self_seed = [0u8; 32];
        self_seed[0] = self_seed0;
        let self_id = NodeId::new(self_seed);
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
    }

    /// The scheduler exits promptly once the cancellation token fires
    /// (cancel is observed on the first `select!` iteration, before any
    /// cadence interval elapses).
    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_exits_on_cancellation() {
        let dht = fresh_dht("sched", 1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), run_scheduler(dht, cancel))
            .await
            .expect("scheduler did not exit after cancellation");
    }

    /// A drift sweep over an empty queue is a no-op (no panic, returns).
    #[tokio::test(flavor = "current_thread")]
    async fn drift_migration_sweep_is_noop_on_empty_plan() {
        let dht = fresh_dht("drift-noop", 2);
        run_drift_migration_sweep(dht).await;
    }
}
