//! Anti-entropy: periodic `MerkleSummary`-driven sync against routing-table
//! peers, bisect-on-mismatch via `MerkleDiff`, fetch missing records.
//!
//! ## Scheduler ([`run_scheduler`])
//!
//! On a `tokio::spawn` task driven by the relay's `CancellationToken`, we
//! interleave three independent sub-tasks at fixed cadences:
//!
//! 1. **Merkle exchange** every [`config::ANTI_ENTROPY_INTERVAL_MS`]:
//!    pick a peer from the routing table at random, send a
//!    `MerkleSummary` over `slices_bitset = self.populated_slices()`,
//!    bisect on each mismatching slice via `MerkleDiff`, fetch the
//!    diverging records via `FetchRecord`, and apply via
//!    [`super::store::store_record`] / `store_tombstone` (with their
//!    canonical conflict-resolution rules).
//!
//! 2. **Eviction sweep** every [`EVICT_INTERVAL_MS`]: scan
//!    `cf_dht_presence` for expired records and drop them. This is
//!    distinct from anti-entropy — it's the "the wall-clock said this
//!    record's `not_after` is past" pass.
//!
//! 3. **Bootstrap retry** when the routing table is sparse
//!    (fewer than [`BOOTSTRAP_RETRY_THRESHOLD`] known peers): re-ask the
//!    resolver. The retry is exponentially-backed-off so a long-down
//!    resolver does not turn the relay into a CPU-soak.
//!
//! Cancellation: every `select!` arm includes
//! `cancel.cancelled().await`; the loop exits cleanly within one
//! cadence-tick of the token firing.
//!

pub(crate) mod merkle;
pub(crate) mod rpc;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::info;
use common::warn;
use tokio_util::sync::CancellationToken;

use super::Dht;
use super::bootstrap::BootstrapError;
use super::bootstrap::bootstrap;
use super::config;

pub(crate) use self::merkle::SliceTree;
pub(crate) use self::merkle::TREE_DEPTH;
pub(crate) use self::merkle::record_value_hash;
pub(crate) use self::merkle::slice_id_for;
// `tombstone_value_hash` will be wired in once a future `FetchTombstone`
// RPC is added; for now the tombstone-aware Merkle entry is removed
// (rather than re-hashed under a tombstone domain) per the note in
// `store.rs::store_tombstone`.
#[allow(unused_imports)]
pub(crate) use self::merkle::tombstone_value_hash;

// ---------------------------------------------------------------------------
// Scheduler tunables
// ---------------------------------------------------------------------------

/// How often we sweep `cf_dht_presence` for `not_after`-expired records.
///
/// Twice the anti-entropy cadence: we only need to evict expired records
/// at roughly the granularity at which their absence becomes
/// observable to peers. A finer cadence wastes IO; coarser would let
/// stale records linger in `FindValue` answers past their TTL.
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

// `MAX_BISECT_DEPTH` lives in `rpc.rs` since the bisect driver is the
// only consumer. The constant equals [`TREE_DEPTH`] (= 4); since the
// wire-format `MerkleDiff::path` is bounded at 4 nibbles, the
// brute-fallback-when-too-deep path is deliberately unreachable in
// v1 — bisect is exhaustive at this tree depth. Documented as a
// follow-up if the tree depth is ever increased.

// ---------------------------------------------------------------------------
// MerkleState — public API
// ---------------------------------------------------------------------------

/// Per-relay anti-entropy state. Lives inside `Dht::merkle` (a
/// `parking_lot::RwLock<MerkleState>`) — write-heavy because every
/// record write/delete updates the slice's leaf-to-root path.
///
/// Storage shape: a `HashMap<u8, SliceTree>` keyed by slice_id. Most
/// slices are empty in steady state; the map is kept small (≈ 1 slice
/// per relay at 10k-relay scale).
#[derive(Debug, Default)]
pub(crate) struct MerkleState {
    /// Per-slice trees, lazily allocated.
    pub trees: HashMap<u8, SliceTree>,
}

impl MerkleState {
    /// Construct an empty state. Slices are populated lazily on first
    /// insert.
    pub(crate) fn empty() -> Self {
        Self { trees: HashMap::new() }
    }

    /// Set the value-hash for `user_ipk`. Re-hashes the affected
    /// leaf-to-root path within the slice. Idempotent on the same
    /// (ipk, value_hash) pair.
    pub(crate) fn insert(&mut self, user_ipk: &[u8; 32], value_hash: [u8; 32]) {
        let sid = slice_id_for(user_ipk);
        let tree = self.trees.entry(sid).or_insert_with(|| SliceTree::new(sid));
        tree.insert(user_ipk, value_hash);
    }

    /// Drop the leaf entry for `user_ipk`. If the entry was the last in
    /// its slice, the now-empty slice tree is removed entirely (so
    /// [`Self::populated_slices_bitset`] doesn't keep advertising it).
    pub(crate) fn remove(&mut self, user_ipk: &[u8; 32]) {
        let sid = slice_id_for(user_ipk);
        let drop = if let Some(tree) = self.trees.get_mut(&sid) {
            tree.remove(user_ipk);
            tree.is_empty()
        } else {
            false
        };
        if drop {
            self.trees.remove(&sid);
        }
    }

    /// Slice root, or `[0; 32]` if the slice is empty / absent.
    pub(crate) fn root(&self, slice_id: u8) -> [u8; 32] {
        self.trees.get(&slice_id).map(|t| t.root()).unwrap_or([0u8; 32])
    }

    /// Compute a 256-bit bitset of populated slices. Bit `i` = 1 iff
    /// `slice i` has at least one record. Sent in
    /// [`MerkleSummary::slices`] so a peer doesn't bother computing
    /// roots for slices we don't even hold.
    ///
    /// **Endianness:** byte 0 holds slice ids 0..8, byte 1 holds 8..16,
    /// etc. Within each byte, bit 0 (LSB) is the lower slice id. So
    /// `slice_id 5` = byte 0, bit 5 = `0b0010_0000`. Same convention as
    /// the [`set_slice_bit`] / [`is_slice_bit_set`] helpers below.
    pub(crate) fn populated_slices_bitset(&self) -> [u8; 32] {
        let mut bs = [0u8; 32];
        for &sid in self.trees.keys() {
            set_slice_bit(&mut bs, sid);
        }
        bs
    }

    /// Build the `MerkleSummary` reply: for each slice id whose bit is
    /// set in `want`, return our `(slice_id, root)`. Empty slices are
    /// omitted (they would all-zero anyway, but it keeps the wire size
    /// bounded by the *minimum* of `popcnt(want)` and our populated
    /// count).
    pub(crate) fn summary(&self, want: &[u8; 32]) -> Vec<(u8, [u8; 32])> {
        let mut out = Vec::new();
        for (&sid, tree) in &self.trees {
            if is_slice_bit_set(want, sid) && !tree.is_empty() {
                out.push((sid, tree.root()));
            }
        }
        // Stable order so two replicas with the same state produce the
        // same response bytes (helps tests + opportunistic byte-level
        // comparisons in operator tooling).
        out.sort_by_key(|(sid, _)| *sid);
        out
    }

    /// Build the `MerkleDiff` response for a `(slice_id, path)` query.
    ///
    /// - Path empty or shorter than [`merkle::TREE_DEPTH`] → return
    ///   `Children { hashes: [16] }`.
    /// - Path of full depth → return `Leaves { entries: [...] }`.
    /// - Slice absent → return `Children` of all-zeros (so a peer that
    ///   thinks the slice is empty agrees with us trivially).
    pub(crate) fn diff(
        &self, slice_id: u8, path: &[u8],
    ) -> common::proto::dht_p2p::MerkleDiffResp {
        use common::proto::dht_p2p::MerkleDiffResp;
        if path.len() >= merkle::TREE_DEPTH {
            // Leaf level — return the (ipk, value_hash) entries.
            let entries = if let Some(tree) = self.trees.get(&slice_id) {
                tree.leaves_at(&path.to_vec())
                    .into_iter()
                    .map(|(ipk, vh)| (ipk.into(), vh.into()))
                    .collect()
            } else {
                Vec::new()
            };
            MerkleDiffResp::Leaves { entries }
        } else {
            // Internal node — return the 16 child hashes.
            let hashes_bytes = if let Some(tree) = self.trees.get(&slice_id) {
                tree.children_at(&path.to_vec())
            } else {
                [[0u8; 32]; merkle::MERKLE_FANOUT]
            };
            let hashes = hashes_bytes.iter().map(|h| (*h).into()).collect();
            MerkleDiffResp::Children { hashes }
        }
    }

    /// Number of populated slices currently held. Used by metrics /
    /// scheduler heuristics, not by the wire protocol.
    pub(crate) fn populated_count(&self) -> usize {
        self.trees.iter().filter(|(_, t)| !t.is_empty()).count()
    }

    /// 16 child hashes at the internal node `(slice_id, path)`. Returns
    /// all-zeros if the slice or the prefix has no records.
    ///
    /// Convenience for the bisect driver — saves a `trees.get` /
    /// `unwrap_or` dance in `rpc.rs`.
    pub(crate) fn children_at(
        &self, slice_id: u8, path: &[u8],
    ) -> [[u8; 32]; merkle::MERKLE_FANOUT] {
        match self.trees.get(&slice_id) {
            Some(t) => t.children_at(&path.to_vec()),
            None => [[0u8; 32]; merkle::MERKLE_FANOUT],
        }
    }

    /// `(user_ipk, value_hash)` entries at the leaf node `(slice_id,
    /// path)`. Empty if the slice or the leaf has no records.
    pub(crate) fn leaves_at(
        &self, slice_id: u8, path: &[u8],
    ) -> Vec<([u8; 32], [u8; 32])> {
        match self.trees.get(&slice_id) {
            Some(t) => t.leaves_at(&path.to_vec()),
            None => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bitset helpers — endianness-explicit
// ---------------------------------------------------------------------------

/// Set `slice_id`'s bit in `bitset`. Layout: `byte = sid / 8`, `bit_in_byte
/// = sid % 8` (LSB-first).
///
/// **Endianness contract:** This is the *one* place the project pins the
/// bitset's byte/bit ordering. Changing it changes the wire format, so
/// every peer must agree. See [`MerkleState::populated_slices_bitset`]
/// for the doc-facing description.
pub(crate) fn set_slice_bit(bitset: &mut [u8; 32], slice_id: u8) {
    let byte = (slice_id / 8) as usize;
    let bit = slice_id % 8;
    bitset[byte] |= 1 << bit;
}

/// Test `slice_id`'s bit in `bitset`. Symmetric to [`set_slice_bit`].
pub(crate) fn is_slice_bit_set(bitset: &[u8; 32], slice_id: u8) -> bool {
    let byte = (slice_id / 8) as usize;
    let bit = slice_id % 8;
    (bitset[byte] & (1 << bit)) != 0
}

/// All-ones bitset — "I'm interested in every slice."
pub(crate) fn all_slices_bitset() -> [u8; 32] {
    [0xFFu8; 32]
}

// ---------------------------------------------------------------------------
// Rebuild-from-records helper
// ---------------------------------------------------------------------------

/// Walk `cf_dht_presence` and rebuild the in-memory `MerkleState` from
/// scratch. Called at relay-startup time before the scheduler comes up,
/// so a freshly-launched binary's roots match the on-disk record set.
///
/// Cost: O(records × TREE_DEPTH × MERKLE_FANOUT). At ~300 records,
/// depth 4, fanout 16 this is < 20k hash ops — well under a millisecond.
///
/// Tombstone entries (33-byte keys with [`super::store::TOMB_PREFIX`])
/// are *not* re-added here: they cease to exist on the network once
/// their honour-window passes, so persisting them in the Merkle tree
/// past restart would diverge replicas that GC'd theirs in the
/// meantime. This may be revisited if tombstone-loss-on-restart proves
/// problematic in practice.
pub(crate) fn rebuild_from_records(dht: &Dht) -> usize {
    let mut count = 0usize;
    let mut merkle = dht.merkle.write();
    *merkle = MerkleState::empty();

    for guard in dht.store.presence.iter() {
        let (key, value) = match guard.into_inner() {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        // Only bare 32-byte keys are presence records (tombstones use
        // a 33-byte prefixed key).
        if key.len() != 32 {
            continue;
        }
        let mut ipk = [0u8; 32];
        ipk.copy_from_slice(&key);

        // value_hash is BLAKE3 of the postcard-serialised record bytes
        // — same recipe `store_record` uses on the live path.
        let vh = record_value_hash(&value);
        merkle.insert(&ipk, vh);
        count += 1;
    }
    count
}

// ---------------------------------------------------------------------------
// Scheduler — top-level
// ---------------------------------------------------------------------------

/// Wall-clock now in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Anti-entropy + maintenance scheduler. Spawns nothing of its own —
/// the caller is `tokio::spawn(run_scheduler(dht, cancel))`.
///
/// On `cancel.cancelled().await` the loop exits cleanly within one
/// cadence-tick. The function never returns an error: every individual
/// task arm logs and continues so a transient failure (peer down,
/// network blip, fjall error) doesn't kill the entire scheduler.
pub(crate) async fn run_scheduler(dht: Arc<Dht>, cancel: CancellationToken) {
    use tokio::time::interval;

    // Rebuild Merkle state from existing records before we start
    // serving any sync round. A fresh process restart with persistent
    // records would otherwise advertise empty roots and trigger
    // unnecessary anti-entropy churn.
    let rebuilt = rebuild_from_records(&dht);
    if rebuilt > 0 {
        info!("DHT scheduler: rebuilt Merkle state from {rebuilt} records");
    }

    let mut sync_tick = interval(Duration::from_millis(config::ANTI_ENTROPY_INTERVAL_MS));
    let mut evict_tick = interval(Duration::from_millis(EVICT_INTERVAL_MS));
    // `tokio::time::interval` fires once at construction by default;
    // skip that immediate fire so we don't race the bootstrap path.
    sync_tick.tick().await;
    evict_tick.tick().await;

    // Bootstrap-retry state:
    // - `bootstrap_backoff_ms` doubles after each failed retry, capped
    //   at `BOOTSTRAP_RETRY_MAX_BACKOFF_MS`.
    // - `last_bootstrap_attempt_ms` is the wall-clock of the last
    //   *attempt* (not just warning) — we only retry when the backoff
    //   window has fully elapsed. The scheduler actually drives the
    //   bootstrap RPC, not just emits a warning.
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
            _ = sync_tick.tick() => {
                // One sync round per tick. Errors are logged at the
                // call-site, not here — a peer being unreachable is
                // expected churn, not a scheduler bug.
                let _ = rpc::sync_round(dht.clone()).await;

                // Bootstrap-retry: when the routing table is sparse,
                // re-ask the resolver. The handle is `Option`-wrapped
                // on `Dht` so unit-test fixtures (no resolver link)
                // skip this branch silently. See `Dht::attach_resolver`.
                let known = dht.routing.read().total_known();
                let sparse = known < BOOTSTRAP_RETRY_THRESHOLD;
                if sparse && !was_sparse {
                    warn!("DHT routing table sparse ({known} < {BOOTSTRAP_RETRY_THRESHOLD}); retrying bootstrap");
                } else if !sparse && was_sparse {
                    info!("DHT routing table recovered ({known} >= {BOOTSTRAP_RETRY_THRESHOLD})");
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
                                    info!("DHT bootstrap retry succeeded (state {state:?})");
                                    bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
                                },
                                // Brand-new-network case — hold base backoff so a
                                // peer joining shortly after lets us re-converge.
                                Err(BootstrapError::EmptyRegistry) => {
                                    bootstrap_backoff_ms = BOOTSTRAP_RETRY_BASE_MS;
                                },
                                Err(e) => {
                                    warn!("DHT bootstrap retry failed: {e}; backing off");
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
                }
            }
            _ = evict_tick.tick() => {
                let evicted = super::store::evict_expired(&dht, now_ms());
                if evicted > 0 {
                    info!("DHT scheduler: evicted {evicted} expired record(s)");
                }
                // Lazy K-set drift migration. After pruning expired
                // records, sweep `cf_dht_queue` for
                // entries whose recipient is no longer in this
                // relay's K-closest set and migrate them to the new
                // K-closest. Bounded per-sweep by
                // `MAX_MIGRATE_PER_SWEEP` (256 candidates) and
                // `MAX_CONCURRENT_MIGRATIONS` (8 in-flight tasks).
                // Errors are logged at the call-site, not here — a
                // candidate that fails will be retried next sweep.
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
/// without spinning up the full anti-entropy interval loop.
///
/// **Bounded fan-out**:
/// - `MAX_MIGRATE_PER_SWEEP` (256) candidates per sweep — bounds
///   the routing-table-read + fjall-scan cost.
/// - [`config::MAX_CONCURRENT_MIGRATIONS`] (8) in-flight migration
///   tasks — bounds the outbound `Forward` RPC fan-out so a single
///   sweep doesn't open hundreds of QUIC bi-streams at once.
///
/// **Lock contract**: every routing-table read is scoped + cloned
/// out before any `await` (the same project-wide rule
/// `forward_to_homes` follows). The sweep itself doesn't hold any
/// lock across awaits.
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
        // join_next yields when *any* task completes; on completion
        // we submit the next candidate to keep the in-flight count
        // up to the cap.
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
/// `cf_dht_queue` entry. On failure (no homes, insufficient
/// replicas, transient peer churn) the local entry is preserved
/// for the next sweep.
///
/// **Why not propagate the error**: each migration is independent
/// and best-effort; a failure on one candidate must not stall the
/// next one. The metric counters on `Dht::metrics` are the audit
/// surface.
async fn migrate_one(
    dht: Arc<Dht>, key: crate::storage::MessageKey,
    dispatch: common::proto::client_rel::DispatchP, now_ms: u64,
) {
    dht.metrics.inc_migrations_attempted();
    match super::forward::forward_to_homes(dht.clone(), dispatch, now_ms).await {
        Ok(_summary) => {
            // Success → delete the local entry. The deletion is
            // best-effort; a fjall write error gets the entry
            // re-tried next sweep, but the message has already been
            // durably stored at the new K-closest, so duplicate
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
    use super::*;

    #[test]
    fn merkle_state_empty_root_is_zero() {
        let s = MerkleState::empty();
        assert_eq!(s.root(0), [0u8; 32]);
        assert_eq!(s.populated_count(), 0);
    }

    #[test]
    fn merkle_state_insert_then_remove_returns_to_zero_root() {
        let mut s = MerkleState::empty();
        let mut k = [0u8; 32];
        k[0] = 5;
        k[1] = 0x12;
        s.insert(&k, [42u8; 32]);
        assert_ne!(s.root(5), [0u8; 32]);

        s.remove(&k);
        assert_eq!(s.root(5), [0u8; 32]);
        assert_eq!(s.populated_count(), 0);
    }

    #[test]
    fn merkle_state_two_inserts_in_same_slice_diff_keys() {
        // Same content, different insert order → identical root.
        let mut k1 = [0u8; 32];
        k1[0] = 5;
        k1[1] = 0x11;
        let mut k2 = [0u8; 32];
        k2[0] = 5;
        k2[1] = 0x22;

        let mut a = MerkleState::empty();
        a.insert(&k1, [1u8; 32]);
        a.insert(&k2, [2u8; 32]);

        let mut b = MerkleState::empty();
        b.insert(&k2, [2u8; 32]);
        b.insert(&k1, [1u8; 32]);

        assert_eq!(a.root(5), b.root(5));
        assert_ne!(a.root(5), [0u8; 32]);
    }

    #[test]
    fn populated_slices_bitset_marks_only_populated_slices() {
        let mut s = MerkleState::empty();
        let mut k = [0u8; 32];
        k[0] = 5;
        s.insert(&k, [1u8; 32]);
        let mut k2 = [0u8; 32];
        k2[0] = 200;
        s.insert(&k2, [2u8; 32]);

        let bs = s.populated_slices_bitset();
        assert!(is_slice_bit_set(&bs, 5));
        assert!(is_slice_bit_set(&bs, 200));
        assert!(!is_slice_bit_set(&bs, 4));
        assert!(!is_slice_bit_set(&bs, 6));
        assert!(!is_slice_bit_set(&bs, 199));
    }

    #[test]
    fn summary_filters_by_bitset() {
        let mut s = MerkleState::empty();
        let mut k1 = [0u8; 32];
        k1[0] = 5;
        s.insert(&k1, [1u8; 32]);
        let mut k2 = [0u8; 32];
        k2[0] = 7;
        s.insert(&k2, [2u8; 32]);

        // Bitset asks only for slice 5.
        let mut want = [0u8; 32];
        set_slice_bit(&mut want, 5);

        let r = s.summary(&want);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 5);
        assert_ne!(r[0].1, [0u8; 32]);
    }

    #[test]
    fn diff_at_root_returns_children_variant() {
        use common::proto::dht_p2p::MerkleDiffResp;
        let mut s = MerkleState::empty();
        let mut k = [0u8; 32];
        k[0] = 5;
        k[1] = 0xA0; // first nibble 0xA
        s.insert(&k, [1u8; 32]);

        match s.diff(5, &[]) {
            MerkleDiffResp::Children { hashes } => {
                assert_eq!(hashes.len(), merkle::MERKLE_FANOUT);
                assert_ne!(hashes[0xA], [0u8; 32].into());
                for (i, h) in hashes.iter().enumerate() {
                    if i != 0xA {
                        assert_eq!(h, &[0u8; 32].into());
                    }
                }
            }
            MerkleDiffResp::Leaves { .. } => panic!("expected Children at root depth"),
        }
    }

    #[test]
    fn diff_at_leaf_depth_returns_leaves_variant() {
        use common::proto::dht_p2p::MerkleDiffResp;
        let mut s = MerkleState::empty();
        let mut k = [0u8; 32];
        k[0] = 5;
        k[1] = 0xA0;
        s.insert(&k, [1u8; 32]);

        let path = vec![0xA, 0x0, 0x0, 0x0];
        match s.diff(5, &path) {
            MerkleDiffResp::Leaves { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.0, k);
                assert_eq!(entries[0].1.0, [1u8; 32]);
            }
            MerkleDiffResp::Children { .. } => panic!("expected Leaves at full depth"),
        }
    }

    #[test]
    fn diff_for_unknown_slice_returns_zero_children() {
        use common::proto::dht_p2p::MerkleDiffResp;
        let s = MerkleState::empty();
        match s.diff(99, &[]) {
            MerkleDiffResp::Children { hashes } => {
                assert_eq!(hashes.len(), merkle::MERKLE_FANOUT);
                for h in &hashes {
                    assert_eq!(h, &[0u8; 32].into());
                }
            }
            other => panic!("expected Children, got {other:?}"),
        }
    }

    /// Smoke-test the scheduler's cancellation path: spawn it on a
    /// fresh `Dht` (with no peers) and verify that cancelling the
    /// token causes the loop to exit promptly. The full
    /// peer-driven sync behaviour is integration territory.
    ///
    /// `start_paused = true` would let us pin virtual time (and require
    /// the `tokio/test-util` feature); without it we rely on the fact
    /// that cancellation triggers a `select!` arm that resolves
    /// immediately, well ahead of the 30-second sync cadence.
    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_exits_on_cancellation() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering as AtomicOrdering;

        use ed25519_dalek::SigningKey;
        use tokio_util::sync::CancellationToken;

        use crate::dht::Dht;
        use crate::dht::DhtConfig;
        use common::quic::id::NodeId;

        // Minimal fixture inline — we don't share `fresh_dht` across
        // sync/* tests because the path-suffix counter would collide
        // with parallel test runs.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-sched-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let cfg = DhtConfig::default();
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"));

        let cancel = CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let handle = tokio::spawn(async move {
            run_scheduler(dht, cancel_for_task).await;
        });

        // Yield once so the scheduler has a chance to start, then cancel.
        // With `start_paused = true`, real time doesn't advance, so the
        // `interval` ticks won't fire — the only awaitable that resolves
        // is the cancellation.
        tokio::task::yield_now().await;
        cancel.cancel();

        // The scheduler should exit promptly. Bound on a generous
        // timeout (still < 2s per the dispatch's per-test budget).
        tokio::time::timeout(Duration::from_secs(1), handle).await.expect("scheduler should exit").expect("task ok");
    }

    #[test]
    fn slice_bitset_endianness_is_lsb_first() {
        // Pin the LSB-first contract so a future refactor that
        // accidentally flips it surfaces here, not in production.
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, 0);
        assert_eq!(bs[0], 0b0000_0001);
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, 5);
        assert_eq!(bs[0], 0b0010_0000);
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, 8);
        assert_eq!(bs[1], 0b0000_0001);
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, 255);
        assert_eq!(bs[31], 0b1000_0000);
    }

    /// Verify the `run_drift_migration_sweep` plumbing actually feeds
    /// candidates from `plan_drift_migrations` into the migration driver
    /// and bumps `migrations_attempted` on each candidate. We can't test
    /// the actual outbound `Forward` RPC outcome (that needs a live
    /// two-relay harness, future work) — only the wiring: scheduler →
    /// planner → driver → metrics counter, with the per-candidate cap
    /// honoured.
    ///
    /// **Setup**: a `Dht` whose `self_id = [0xFF; 32]` is far from a
    /// recipient at all-zero IPK; install K=3 peers strictly closer
    /// to all-zero so the planner's drift check fires for every
    /// queued entry.
    ///
    /// **Assertion**: after one sweep, the `migrations_attempted`
    /// counter equals the number of queued entries (capped by
    /// `MAX_MIGRATE_PER_SWEEP`). The actual outbound forwards will
    /// fail (no live peers), so `migrations_failed` ≥
    /// `migrations_succeeded`. We don't assert on the success/fail
    /// split — the load-bearing piece is that the planner's
    /// candidates were indeed dispatched.
    #[tokio::test(flavor = "current_thread")]
    async fn drift_migration_sweep_dispatches_planned_candidates() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering as AtomicOrdering;

        use common::proto::client_rel::DispatchP;
        use common::proto::client_rel::dispatch_sig_message;
        use common::proto::dht_p2p::NodeDescriptor;
        use common::quic::id::NodeId;
        use ed25519_dalek::Signer;
        use ed25519_dalek::SigningKey;

        use crate::dht::Dht;
        use crate::dht::DhtConfig;

        // Inline `fresh_dht` analogue (sync/* tests don't share with
        // store.rs's helper to avoid path-counter cross-talk).
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-mig-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let cfg = DhtConfig::default();
        // self far from a 0-prefix target
        let mut self_seed = [0u8; 32];
        self_seed[0] = 0xFF;
        let self_id = NodeId::new(self_seed);
        let dht = Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"));

        // Install K=3 peers strictly closer to a 0-prefix target.
        for i in 0..3u8 {
            let mut s = [0u8; 32];
            s[31] = i;
            let pid = NodeId::new(s);
            let desc = NodeDescriptor {
                id:     pid,
                addr:   "127.0.0.1:1".parse().unwrap(),
                pubkey: [0u8; 32].into(),
            };
            dht.routing.write().insert(desc);
        }

        // Queue a few dispatches for an all-zero recipient. Self is
        // now drifted out of K-closest for that recipient → planner
        // returns these.
        let from_user = SigningKey::from_bytes(&[42u8; 32]);
        let from_ipk: [u8; 32] = from_user.verifying_key().to_bytes();
        let to_ipk: [u8; 32] = [0u8; 32];
        let queued = 5u8;
        for i in 0..queued {
            let mut id_bytes = [0u8; 16];
            id_bytes[0] = i;
            let msg = dispatch_sig_message(&to_ipk, &from_ipk, &id_bytes, b"drift");
            let sig = from_user.sign(&msg);
            let dispatch = DispatchP {
                to:      to_ipk.into(),
                from:    from_ipk.into(),
                id:      id_bytes.into(),
                payload: b"drift".to_vec().into(),
                sig:     sig.to_bytes().into(),
            };
            super::super::store::enqueue_for_home(&dht, &to_ipk, &dispatch, 100 + i as u64);
        }

        // Sanity: the planner indeed sees `queued` candidates.
        let candidates =
            super::super::store::plan_drift_migrations(&dht, 16);
        assert_eq!(candidates.len(), queued as usize, "planner sees all queued");

        // Drive one sweep. Outbound `Forward` RPCs will fail (no
        // live peers), but the wiring should still bump
        // `migrations_attempted` once per candidate.
        super::run_drift_migration_sweep(dht.clone()).await;

        let attempted = dht
            .metrics
            .migrations_attempted
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            attempted, queued as u64,
            "every planner candidate should reach the migration driver"
        );

        // Failures dominate (no peers reachable); no successes.
        let failed = dht
            .metrics
            .migrations_failed
            .load(std::sync::atomic::Ordering::Relaxed);
        let succeeded = dht
            .metrics
            .migrations_succeeded
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            failed + succeeded,
            attempted,
            "every attempted migration must end in success or failure"
        );
        // Without live peers, the lone-relay shortcut in
        // `forward_to_homes` enqueues self-store but K_MIN=2 fails
        // → ForwardError → migrations_failed bump.
        assert!(
            failed > 0,
            "with no live peers, at least one migration must fail"
        );
    }

    /// When the planner returns no candidates (the steady-state case),
    /// the sweep is a no-op and nothing bumps.
    /// Catches a regression where the sweep accidentally bumps
    /// metrics on every tick regardless of work.
    #[tokio::test(flavor = "current_thread")]
    async fn drift_migration_sweep_is_noop_on_empty_plan() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering as AtomicOrdering;

        use common::quic::id::NodeId;
        use ed25519_dalek::SigningKey;

        use crate::dht::Dht;
        use crate::dht::DhtConfig;

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path =
            std::env::temp_dir().join(format!("promtuz-mig-noop-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let cfg = DhtConfig::default();
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"));

        super::run_drift_migration_sweep(dht.clone()).await;
        let attempted = dht
            .metrics
            .migrations_attempted
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(attempted, 0, "empty queue → no attempts");
    }
}
