//! Kademlia-style 256-bucket routing table over the unified
//! `NodeId`/`IPK` keyspace.
//!
//! This module implements the in-memory routing-table state: bucket
//! geometry, refresh, eviction, and learning. The lookup loop in
//! `super::lookup` and the bootstrap state machine in `super::bootstrap`
//! read/write through these methods.
//!
//! ## Lock contract
//!
//! [`RoutingTable`] is held inside a single `parking_lot::RwLock` on
//! [`super::Dht::routing`]. Every method takes `&self` or `&mut self`
//! directly — the caller manages the lock and **never** holds it across
//! `await` (project-wide rule).
//!
//! ## Metrics
//!
//! Eviction-relevant methods return rich outcome enums rather than
//! reaching into [`super::metrics::Metrics`]. The caller (the
//! handler-side ping path) inspects the outcome and bumps the relevant
//! counter — keeps the routing table free of side-channel dependencies
//! and trivially unit-testable.

use std::net::SocketAddr;
use std::time::Instant;

use common::proto::dht_p2p::NodeDescriptor;
use common::quic::id::NodeId;
use common::quic::xor32;
use quinn::Connection;

use super::Dht;
use super::config::BUCKETS;
use super::config::BUCKET_SIZE;
use super::config::K;

// ---------------------------------------------------------------------------
// Module-private constants
// ---------------------------------------------------------------------------

/// Failed-`PING` count after which the LRU entry is evicted from its
/// bucket and (if any) the head of `candidates` is promoted into its
/// slot.
///
/// We accept the standard Kademlia robustness extension of *three
/// consecutive* timeouts before declaring a peer dead, which absorbs
/// transient network glitches without changing the observable behaviour
/// for stably-up peers.
pub(crate) const PING_FAILURES_BEFORE_EVICTION: u8 = 3;

/// Smoothing factor for the RTT exponentially-weighted moving average:
/// new_ema = (old_ema * (DENOM-1) + sample) / DENOM, i.e. 1/8 weight on
/// the most recent sample.
///
/// 1/8 is the same smoothing factor TCP-RTO uses for its SRTT, a
/// well-tested default with bounded sensitivity to single-sample spikes.
/// Vivaldi is future work.
const RTT_EMA_DENOM: u32 = 8;

// ---------------------------------------------------------------------------
// RoutingEntry
// ---------------------------------------------------------------------------

/// Single peer entry stored in a [`Bucket`].
///
/// `conn` is a `std::sync::Weak` reference so that `Bucket`-walking code
/// can attempt to reuse an open relay-to-relay connection without holding
/// the lock that owns the strong handle. If the connection has been
/// dropped (peer died, idle-timeout fired) the upgrade returns `None` and
/// the dialer-path opens a fresh QUIC handshake.
#[derive(Debug, Clone)]
pub struct RoutingEntry {
    /// Peer NodeId — full 32 bytes.
    pub id: NodeId,

    /// Last-known socket address. Updated when an inbound RPC arrives from
    /// a different `addr` for the same `id` (peer roamed networks).
    pub addr: SocketAddr,

    /// Verified Ed25519 pubkey from the cert chain. The peer's TLS leaf
    /// cert is signed by `relay_ca` / tier{1,2}_relay_ca; the SPKI is the
    /// Ed25519 pubkey, and `BLAKE3(spki) == id` is checked on first contact.
    pub pubkey: [u8; 32],

    /// Last point in time this peer was observed alive (any inbound RPC
    /// reply or unsolicited request). Drives the bucket-LRU ordering.
    /// Stored as `Instant` rather than ms-since-epoch so monotonicity is
    /// guaranteed regardless of wall-clock jumps.
    pub last_seen: Instant,

    /// Exponentially-weighted moving average of measured RTTs in ms.
    /// `None` means "never measured" (e.g. just learned from a peer's
    /// `FindNodeResp` and we haven't issued our speculative `PING` yet).
    ///
    pub rtt_ema_ms: Option<u32>,

    /// Consecutive failed-`PING` counter. Reset on a successful PING or
    /// any other inbound RPC reply. Reaching
    /// [`PING_FAILURES_BEFORE_EVICTION`] triggers eviction and (if any)
    /// promotion of the head of `Bucket::candidates`.
    ///
    /// `u8` is plenty — anything past 3 is noise.
    pub failed_pings: u8,

    /// Hot connection if available. `Weak<Connection>` so the routing
    /// table never *owns* the connection — the strong reference lives on
    /// `Dht::peer_conns`. Lookup hops `upgrade()` opportunistically.
    pub conn: Option<std::sync::Weak<Connection>>,
}

impl RoutingEntry {
    /// Materialise the wire-shaped descriptor (id, addr, pubkey) for
    /// inclusion in a `FindNodeResp` / `FindValueOutcome::Closer`.
    ///
    /// Allocates a new `NodeDescriptor` rather than borrowing because
    /// `NodeDescriptor::pubkey: Bytes<32>` is owned and the routing entry
    /// must outlive the response on its own LRU-driven schedule.
    pub(crate) fn descriptor(&self) -> NodeDescriptor {
        NodeDescriptor {
            id:     self.id,
            addr:   self.addr,
            pubkey: self.pubkey.into(),
        }
    }

    /// Construct a freshly-learned entry from a wire descriptor (e.g. an
    /// element of a peer's `FindNodeResp`). `last_seen` is set to "now"
    /// because the peer that named this descriptor was itself recently
    /// alive — a weak signal but the best we have until our own PING
    /// completes.
    pub(crate) fn from_descriptor(desc: &NodeDescriptor) -> Self {
        Self {
            id:           desc.id,
            addr:         desc.addr,
            pubkey:       desc.pubkey.0,
            last_seen:    Instant::now(),
            rtt_ema_ms:   None,
            failed_pings: 0,
            conn:         None,
        }
    }
}

// ---------------------------------------------------------------------------
// Bucket
// ---------------------------------------------------------------------------

/// One k-bucket in the routing table.
///
/// Bucket invariants (enforced by methods, asserted in tests):
/// - `entries.len() <= BUCKET_SIZE`
/// - `entries[0]` is the least-recently-seen peer (head of LRU).
/// - `entries[len-1]` is the most-recently-seen peer.
/// - `candidates.len() <= BUCKET_SIZE`; same ordering convention.
///
#[derive(Debug)]
pub struct Bucket {
    /// Active peers, head = LRU.
    ///
    /// `Vec` not `SmallVec`: adding a dependency for ~16 entries isn't
    /// worth it. The per-bucket peak is `BUCKET_SIZE`, so a
    /// `Vec::with_capacity(B)` never reallocates at runtime anyway.
    pub entries: Vec<RoutingEntry>,

    /// Last point in time something in this bucket was touched. Used by
    /// the refresh job to decide which buckets to re-discover via
    /// `FindNode` against a random key in their range.
    ///
    pub refresh_at: Instant,

    /// Replacement cache — would-be inserts that arrived while the bucket
    /// was full. Promoted to `entries` when an active entry fails its
    /// liveness probe.
    ///
    /// Capped at `BUCKET_SIZE`: an unbounded `candidates` vec would let
    /// an attacker force unbounded memory growth by spraying fresh
    /// peer-descriptor RPCs at a full bucket.
    pub candidates: Vec<RoutingEntry>,
}

impl Bucket {
    pub(crate) fn empty(now: Instant) -> Self {
        Self {
            entries:    Vec::with_capacity(BUCKET_SIZE),
            refresh_at: now,
            candidates: Vec::with_capacity(BUCKET_SIZE),
        }
    }
}

// ---------------------------------------------------------------------------
// InsertOutcome
// ---------------------------------------------------------------------------

/// Caller-observable result of a [`RoutingTable::insert`] call.
///
/// Each variant tells the caller exactly what (if anything) it should do
/// next on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The descriptor's id matched [`RoutingTable::self_id`]; we never
    /// store ourselves in our own routing table. No-op.
    IsSelf,

    /// Peer was already known; we updated `last_seen` and bumped it to
    /// the tail (most-recently-seen).
    Refreshed,

    /// Peer was previously unknown; appended to the tail of its bucket.
    Inserted,

    /// Peer was previously unknown but its bucket is full. The descriptor
    /// is now in the bucket's `candidates` cache, awaiting a slot. The
    /// caller **must** issue a `PING` against the LRU entry returned with
    /// this variant — on success the candidate stays parked, on
    /// [`PING_FAILURES_BEFORE_EVICTION`] consecutive failures
    /// [`RoutingTable::ping_failed`] evicts the LRU and promotes the
    /// candidate.
    PendingPing(NodeDescriptor),

    /// Peer was previously unknown, the bucket is full, and the
    /// `candidates` cache is also full. The new descriptor is dropped
    /// rather than thrash the cache (an attacker-driven shuffle attack
    /// would otherwise displace good replacements with junk on every
    /// inbound packet).
    Discarded,
}

// ---------------------------------------------------------------------------
// PingOutcome
// ---------------------------------------------------------------------------

/// Result of [`RoutingTable::ping_failed`].
///
/// Returned rather than calling into `Metrics` directly so the routing
/// table stays self-contained (and unit-testable). The caller —
/// the per-peer PING task in the handler — inspects this and bumps
/// `Metrics::bucket_evictions` on the [`Evicted`] /
/// [`EvictedAndPromoted`] arms.
///
///
/// [`Evicted`]: PingFailedOutcome::Evicted
/// [`EvictedAndPromoted`]: PingFailedOutcome::EvictedAndPromoted
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PingFailedOutcome {
    /// Peer was not in the table; nothing to do.
    Unknown,
    /// Failure counter incremented but still below the eviction
    /// threshold.
    Continued,
    /// Peer evicted; no candidate was waiting, so the slot is now empty.
    Evicted,
    /// Peer evicted and the head of `candidates` was promoted into its
    /// slot. The bucket is back at full capacity.
    EvictedAndPromoted,
}

// ---------------------------------------------------------------------------
// RoutingTable
// ---------------------------------------------------------------------------

/// 256-bucket routing table, indexed by the leading-zero count of
/// `self_id ^ peer_id`.
///
/// **Lock granularity.** A single `parking_lot::RwLock<RoutingTable>`
/// lives on `Dht::routing` (read-mostly: every lookup hop reads, only
/// insert/eviction writes). Per the codebase rule, the lock is never
/// held across `await` — clone what you need out of the lock and drop
/// the guard before any I/O.
///
#[derive(Debug)]
pub struct RoutingTable {
    /// Self NodeId. Stored locally so distance computations don't need to
    /// reach back into `Dht`.
    pub self_id: NodeId,

    /// `buckets[i]` holds peers at distance with leading-zero count
    /// `BUCKETS - 1 - i` (so `buckets[BUCKETS-1]` is "shares no leading
    /// bits with us" — the densest bucket). Boxed so the routing table
    /// doesn't move the array on every clone of `RoutingTable`.
    pub buckets: Box<[Bucket; BUCKETS]>,
}

impl RoutingTable {
    /// Construct an empty routing table. All buckets start empty with
    /// `refresh_at = now`.
    pub fn empty(self_id: NodeId) -> Self {
        let now = Instant::now();
        // The macro-y way to build a fixed-size array of non-`Copy` items
        // without depending on `arrayvec` or `array-init`.
        let buckets: Vec<Bucket> = (0..BUCKETS).map(|_| Bucket::empty(now)).collect();
        let buckets: Box<[Bucket; BUCKETS]> = buckets
            .into_boxed_slice()
            .try_into()
            .expect("BUCKETS-sized vec converts to BUCKETS-sized array");
        Self { self_id, buckets }
    }

    /// XOR-distance bucket index for `target` relative to `self_id`.
    ///
    /// Returns `None` only when `target == self_id` (zero distance — no
    /// bucket); otherwise `Some(i)` with `i < BUCKETS`.
    ///
    pub(crate) fn bucket_index(&self, target: &NodeId) -> Option<usize> {
        bucket_for(&self.self_id, target)
    }

    /// Insert (or refresh) a peer in the routing table. The returned
    /// [`InsertOutcome`] tells the caller what to do next on the network;
    /// the routing-table state is fully updated by the time this returns.
    pub fn insert(&mut self, descriptor: NodeDescriptor) -> InsertOutcome {
        let Some(bucket_idx) = bucket_for(&self.self_id, &descriptor.id) else {
            return InsertOutcome::IsSelf;
        };
        let now = Instant::now();
        let bucket = &mut self.buckets[bucket_idx];
        bucket.refresh_at = now;

        // Step 1 — already known? Refresh in-place (move to tail of LRU).
        if let Some(pos) = bucket.entries.iter().position(|e| e.id == descriptor.id) {
            // Update mutable state, then rotate to tail.
            let mut entry = bucket.entries.remove(pos);
            entry.last_seen = now;
            // Address may have shifted (peer roamed networks). Pubkey is
            // identity-bound so we deliberately do NOT update it here —
            // a mismatched pubkey for the same id is a sybil signal caught
            // by the cert-pinning check on first contact.
            entry.addr = descriptor.addr;
            bucket.entries.push(entry);
            return InsertOutcome::Refreshed;
        }

        // Step 2 — spare capacity in the active set? Append.
        if bucket.entries.len() < BUCKET_SIZE {
            bucket.entries.push(RoutingEntry::from_descriptor(&descriptor));
            return InsertOutcome::Inserted;
        }

        // Step 3 — bucket full. The LRU entry is the eviction candidate;
        // park the newcomer in `candidates` and ask the caller to PING
        // the LRU.
        if bucket.candidates.len() < BUCKET_SIZE {
            // Don't double-park the same id.
            if bucket.candidates.iter().any(|c| c.id == descriptor.id) {
                return InsertOutcome::Discarded;
            }
            let lru_descriptor = bucket.entries[0].descriptor();
            bucket.candidates.push(RoutingEntry::from_descriptor(&descriptor));
            return InsertOutcome::PendingPing(lru_descriptor);
        }

        // Step 4 — both `entries` and `candidates` are full. Drop the
        // newcomer rather than thrash the cache (anti-shuffle).
        InsertOutcome::Discarded
    }

    /// Record a failed `PING` against `peer_id`. Increments the entry's
    /// `failed_pings` counter and, on reaching
    /// [`PING_FAILURES_BEFORE_EVICTION`], evicts the entry and promotes
    /// the head of `candidates` if any.
    ///
    pub(crate) fn ping_failed(&mut self, peer_id: &NodeId) -> PingFailedOutcome {
        let Some(bucket_idx) = bucket_for(&self.self_id, peer_id) else {
            return PingFailedOutcome::Unknown;
        };
        let bucket = &mut self.buckets[bucket_idx];
        let Some(pos) = bucket.entries.iter().position(|e| e.id == *peer_id) else {
            return PingFailedOutcome::Unknown;
        };

        let entry = &mut bucket.entries[pos];
        entry.failed_pings = entry.failed_pings.saturating_add(1);
        if entry.failed_pings < PING_FAILURES_BEFORE_EVICTION {
            return PingFailedOutcome::Continued;
        }

        // Evict.
        bucket.entries.remove(pos);

        // Promote the head of `candidates` if any. Initialise its
        // `last_seen` to *now* — the candidate has been parked passively
        // and we treat its newly-active status as a fresh sighting so it
        // doesn't immediately become eviction-bait itself.
        if !bucket.candidates.is_empty() {
            let mut promoted = bucket.candidates.remove(0);
            promoted.last_seen = Instant::now();
            promoted.failed_pings = 0;
            bucket.entries.push(promoted);
            PingFailedOutcome::EvictedAndPromoted
        } else {
            PingFailedOutcome::Evicted
        }
    }

    /// Record a successful `PING` against `peer_id`. Resets the failure
    /// counter, refreshes `last_seen`, and folds `rtt_ms` into the EMA.
    ///
    /// Returns `true` if the peer was in the table (caller can use this
    /// to attribute pong-arrived-after-evict races); `false` if not.
    ///
    pub(crate) fn ping_succeeded(&mut self, peer_id: &NodeId, rtt_ms: u32) -> bool {
        let Some(bucket_idx) = bucket_for(&self.self_id, peer_id) else {
            return false;
        };
        let bucket = &mut self.buckets[bucket_idx];
        let Some(pos) = bucket.entries.iter().position(|e| e.id == *peer_id) else {
            return false;
        };

        let now = Instant::now();
        let entry = &mut bucket.entries[pos];
        entry.failed_pings = 0;
        entry.last_seen = now;
        entry.rtt_ema_ms = Some(match entry.rtt_ema_ms {
            // First sample seeds the EMA directly.
            None => rtt_ms,
            // ema = (ema * (DENOM-1) + sample) / DENOM. `u64` widen so
            // the intermediate multiply can't wrap on a freak large
            // sample; the result fits back into `u32` because each
            // operand does.
            Some(prev) => {
                let prev = u64::from(prev);
                let sample = u64::from(rtt_ms);
                let denom = u64::from(RTT_EMA_DENOM);
                ((prev * (denom - 1) + sample) / denom) as u32
            }
        });

        // Move to tail (most-recently-seen).
        let entry = bucket.entries.remove(pos);
        bucket.entries.push(entry);
        bucket.refresh_at = now;
        true
    }

    /// Pick the top-`count` peers closest (by XOR) to `target`.
    ///
    /// Returns wire-shaped [`NodeDescriptor`]s rather than full
    /// [`RoutingEntry`]s because the only public consumer (FIND_NODE
    /// response construction) needs the wire form. Use
    /// [`Self::closest`] for the routing-internal full-entry variant.
    ///
    pub(crate) fn find_closest(&self, target: &NodeId, count: usize) -> Vec<NodeDescriptor> {
        self.closest(target, count).into_iter().map(|e| e.descriptor()).collect()
    }

    /// Routing-internal variant of [`Self::find_closest`] that yields the
    /// full [`RoutingEntry`]s (needed by the iterative-lookup driver for
    /// the `Weak<Connection>` reuse and the RTT-EMA tiebreak).
    ///
    /// Routing tables are small (`BUCKETS * BUCKET_SIZE = 4096` entries
    /// max), so a plain full-table scan + sort is fine. A
    /// `BinaryHeap`-of-k would shave a constant factor but is harder to
    /// reason about, and not worth it at this size.
    pub(crate) fn closest(&self, target: &NodeId, count: usize) -> Vec<RoutingEntry> {
        if count == 0 {
            return Vec::new();
        }
        let target_bytes = target.as_bytes();

        // Single pass: collect (distance, &entry) pairs.
        let mut scratch: Vec<([u8; 32], &RoutingEntry)> = Vec::new();
        for bucket in self.buckets.iter() {
            for entry in &bucket.entries {
                let dist = xor_bytes(entry.id.as_bytes(), target_bytes);
                scratch.push((dist, entry));
            }
        }
        scratch.sort_by_key(|(a, _)| *a);
        scratch.truncate(count);
        scratch.into_iter().map(|(_, e)| e.clone()).collect()
    }

    /// Total active-entry count across all buckets. Used by the
    /// bootstrap state machine's "warming" threshold.
    pub(crate) fn total_known(&self) -> usize {
        self.buckets.iter().map(|b| b.entries.len()).sum()
    }

    /// Indices of buckets whose `refresh_at` is older than
    /// `now - BUCKET_REFRESH_MS`, in ascending order. The caller (the
    /// periodic refresh scheduler) issues a self-FindNode against a
    /// random target whose `bucket_for(self, target)` matches each, then
    /// calls [`Self::mark_refreshed`] on each successful walk.
    ///
    /// We deliberately don't update `refresh_at` here — an aborted
    /// refresh would otherwise loop forever, since the scheduler would
    /// see a fresh timestamp and skip the retry.
    ///
    pub(crate) fn buckets_needing_refresh(&self, now: Instant) -> Vec<usize> {
        let threshold = std::time::Duration::from_millis(super::config::BUCKET_REFRESH_MS);
        let mut out = Vec::new();
        for (idx, bucket) in self.buckets.iter().enumerate() {
            // `Instant::checked_duration_since` returns `None` when the
            // argument is in the future, which can't happen here — every
            // `refresh_at` was set by an earlier `Instant::now()`. Treat
            // the `None` case defensively as "not stale".
            if let Some(age) = now.checked_duration_since(bucket.refresh_at)
                && age >= threshold {
                    out.push(idx);
                }
        }
        out
    }

    /// Marks `bucket_idx` as freshly refreshed. Called by the refresh
    /// scheduler after the corresponding self-FindNode walk completes
    /// successfully. Safe to call with an out-of-range `bucket_idx`
    /// (no-op) — keeps the caller boilerplate-free even if a bucket
    /// disappeared from a future restructuring.
    ///
    pub(crate) fn mark_refreshed(&mut self, bucket_idx: usize) {
        if let Some(bucket) = self.buckets.get_mut(bucket_idx) {
            bucket.refresh_at = Instant::now();
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions: distance / bucket math
// ---------------------------------------------------------------------------

/// Compute the bucket index for `(self_id, peer_id)`.
///
/// Returns `None` when `self_id == peer_id` (zero distance — no bucket).
/// Otherwise returns `Some(i)` where `i = 255 - leading_zero_bits(XOR)`
/// and `i ∈ [0, BUCKETS)`.
///
/// Worked examples (with `BUCKETS = 256`):
/// - peers differing in the MSB → XOR has 0 leading zeros → bucket 255.
/// - peers differing only in the LSB's last bit → XOR has 255 leading
///   zeros → bucket 0.
pub(crate) fn bucket_for(self_id: &NodeId, peer_id: &NodeId) -> Option<usize> {
    let xor = xor_bytes(self_id.as_bytes(), peer_id.as_bytes());

    // Find the leading-zero bit count. Iterate big-endian byte by byte
    // because `u256` isn't a native type and pulling in `num-bigint`
    // for one operation is overkill.
    let mut lzc: usize = 0;
    for &b in xor.iter() {
        if b == 0 {
            lzc += 8;
        } else {
            lzc += b.leading_zeros() as usize;
            break;
        }
    }

    if lzc == 256 {
        // All bytes were zero — `self_id == peer_id`.
        None
    } else {
        // `lzc ∈ [0, 255]`, so `255 - lzc ∈ [0, 255]`, fits in `usize`.
        Some(BUCKETS - 1 - lzc)
    }
}

/// Byte-wise XOR of two 32-byte ids. Used both by the bucket-index
/// computation and the `find_closest` distance metric.
#[inline]
fn xor_bytes(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

// ---------------------------------------------------------------------------
// Self-in-top-K helper (shared by store / handler / mls::kp / mls::welcome)
// ---------------------------------------------------------------------------

/// True iff `dht.node_id` is among the K closest to `target` under the
/// current routing-table view.
///
/// This is the single canonical implementation of the "is this relay
/// one of the K-set owners?" check used by the four sticky-home /
/// MLS-stash handlers (`store::self_is_owner`,
/// `handler::self_in_top_k`, `mls::kp::self_is_owner_for_stash`,
/// `mls::welcome::self_is_owner_for_recipient`).
///
/// **Permissive sparse-table policy**: if the routing table holds
/// fewer than `K` candidates we return `true` rather than `false`.
/// A relay that just bootstrapped is allowed to accept stores/publishes
/// before its routing table is dense — otherwise we couldn't seed
/// a fresh network. Migration sweeps re-balance later.
///
/// We query for `K + 1` rather than `K` so that a self-equal-distance
/// entry at the K-th position cannot be tiebroken out and silently
/// drop us out of the K-set.
///
/// **Lock contract**: takes the routing-table read lock briefly to
/// snapshot the candidates; never held across `await` (this function
/// is sync).
pub(crate) fn self_in_top_k(dht: &Dht, target: &NodeId) -> bool {
    let candidates = dht.routing.read().find_closest(target, K + 1);
    if candidates.len() < K {
        return true;
    }
    let target_bytes = target.as_bytes();
    let self_dist = xor32(dht.node_id.as_bytes(), target_bytes);
    let kth_dist = xor32(candidates[K - 1].id.as_bytes(), target_bytes);
    self_dist <= kth_dist
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use common::proto::dht_p2p::NodeDescriptor;
    use common::quic::id::NodeId;

    use super::*;

    // -------- helpers ---------------------------------------------------

    /// Deterministic pubkey-derived NodeId from a 32-byte seed.
    /// `NodeId::new` runs BLAKE3 over the seed, so distinct seeds yield
    /// distinct ids with overwhelming probability.
    fn id_from_seed(seed: [u8; 32]) -> NodeId {
        NodeId::new(seed)
    }

    /// `id_from_seed` of a single varying byte — handy for small
    /// pseudo-random fixtures without an RNG dep.
    fn id_n(n: u8) -> NodeId {
        let mut seed = [0u8; 32];
        seed[0] = n;
        NodeId::new(seed)
    }

    /// Build a `NodeDescriptor` for an id, with a placeholder addr.
    fn desc(id: NodeId) -> NodeDescriptor {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        NodeDescriptor {
            id,
            addr,
            pubkey: [0u8; 32].into(),
        }
    }

    /// Make a routing table whose self-id is `seed`-derived. Tests call
    /// `bucket_for(&table.self_id, &peer)` to discover which bucket a
    /// generated peer lands in (we don't try to engineer specific
    /// bucket placements — that's both fragile and unnecessary).
    fn fresh_table(self_seed: u8) -> RoutingTable {
        RoutingTable::empty(id_n(self_seed))
    }

    /// Search a brute-force range of trial seeds, starting from
    /// `*cursor` and bumping it past the chosen seed, until we find an
    /// id whose `bucket_for(self, id)` is `target_bucket`. The cursor
    /// makes successive calls return *distinct* ids — without it the
    /// search is deterministic-given-self-id and would loop forever in
    /// `if filled.contains(&id) { continue; }`-style callers.
    ///
    /// Bounded so a pathological self-id can't loop forever.
    fn id_in_bucket(self_id: &NodeId, target_bucket: usize, cursor: &mut u32) -> NodeId {
        let start = *cursor;
        for offset in 0u32..1_000_000 {
            let n = start.wrapping_add(offset);
            let mut seed = [0u8; 32];
            seed[..4].copy_from_slice(&n.to_le_bytes());
            // Bias the high byte too so we sweep the bit-distance
            // distribution faster.
            seed[31] = (n & 0xff) as u8;
            let id = NodeId::new(seed);
            if let Some(b) = bucket_for(self_id, &id)
                && b == target_bucket {
                    *cursor = n.wrapping_add(1);
                    return id;
                }
        }
        panic!("could not find id mapping to bucket {target_bucket}")
    }

    /// Fill `bucket` in `t` with `BUCKET_SIZE` distinct peers, asserting
    /// each insert returns `Inserted`. Returns the ids in insertion order
    /// (so `[0]` is the LRU head). Shared by the bucket-full insert,
    /// candidate-slot, and eviction/promotion tests.
    fn fill_bucket(t: &mut RoutingTable, bucket: usize, cursor: &mut u32) -> Vec<NodeId> {
        let mut filled: Vec<NodeId> = Vec::new();
        while filled.len() < BUCKET_SIZE {
            let id = id_in_bucket(&t.self_id, bucket, cursor);
            if filled.contains(&id) {
                continue;
            }
            filled.push(id);
            assert_eq!(t.insert(desc(id)), InsertOutcome::Inserted);
        }
        filled
    }

    // -------- bucket_for math ------------------------------------------

    #[test]
    fn bucket_for_msb_diff_returns_max_bucket() {
        // Construct two ids that differ in the MSB. We can't ask for
        // "BLAKE3 outputs that differ in the MSB" directly, so build
        // them via `from_bytes` and skip the hash.
        let a_bytes = [0u8; 32];
        let mut b_bytes = [0u8; 32];
        b_bytes[0] = 0b1000_0000;
        let a = NodeId::from_bytes(a_bytes);
        let b = NodeId::from_bytes(b_bytes);

        // XOR has byte0 = 0b1000_0000 → 0 leading zeros → bucket 255.
        assert_eq!(bucket_for(&a, &b), Some(BUCKETS - 1));
    }

    #[test]
    fn bucket_for_lsb_diff_returns_zero_bucket() {
        let a_bytes = [0u8; 32];
        let mut b_bytes = [0u8; 32];
        b_bytes[31] = 0x01; // differ only in the very last bit.
        let a = NodeId::from_bytes(a_bytes);
        let b = NodeId::from_bytes(b_bytes);

        // XOR has 255 leading zeros → bucket 0.
        assert_eq!(bucket_for(&a, &b), Some(0));
    }

    #[test]
    fn bucket_for_self_returns_none() {
        let a = id_n(7);
        assert_eq!(bucket_for(&a, &a), None);
    }

    // -------- insert outcomes ------------------------------------------

    #[test]
    fn insert_returns_inserted_for_new_peer() {
        let mut t = fresh_table(0);
        let p = id_n(1);
        // sanity: p is not the self_id.
        assert_ne!(p, t.self_id);
        assert_eq!(t.insert(desc(p)), InsertOutcome::Inserted);
        assert_eq!(t.total_known(), 1);
    }

    #[test]
    fn insert_returns_refreshed_for_seen_peer() {
        let mut t = fresh_table(0);
        let p = id_n(1);
        assert_eq!(t.insert(desc(p)), InsertOutcome::Inserted);
        assert_eq!(t.insert(desc(p)), InsertOutcome::Refreshed);
        // Refresh must not double-count.
        assert_eq!(t.total_known(), 1);
    }

    #[test]
    fn insert_returns_pending_ping_when_bucket_full() {
        let mut t = fresh_table(0);
        let mut cursor = 0u32;
        // Pick any non-empty bucket — bucket 255 is overwhelmingly likely
        // to be the one a uniformly-random peer-id lands in (since it
        // requires *no* shared leading bits), but engineering against
        // probability is fragile. Instead force a target bucket and
        // populate it past BUCKET_SIZE via `id_in_bucket`.
        let target_bucket = 255;
        let filled = fill_bucket(&mut t, target_bucket, &mut cursor);

        // The (B+1)th insert into the same bucket must yield PendingPing.
        // Loop until we find a fresh id in the same bucket that isn't
        // already in `filled`.
        loop {
            let id = id_in_bucket(&t.self_id, target_bucket, &mut cursor);
            if filled.contains(&id) {
                continue;
            }
            match t.insert(desc(id)) {
                InsertOutcome::PendingPing(lru) => {
                    // The carried descriptor must be the LRU — i.e. the
                    // first peer we inserted (still at entries[0]).
                    assert_eq!(lru.id, filled[0]);
                    break;
                }
                other => panic!("expected PendingPing, got {other:?}"),
            }
        }
    }

    #[test]
    fn insert_returns_discarded_when_replacement_slot_taken() {
        let mut t = fresh_table(0);
        let mut cursor = 0u32;
        let target_bucket = 255;

        // Fill `entries`.
        let active = fill_bucket(&mut t, target_bucket, &mut cursor);

        // Fill `candidates` with another B distinct ids, each yielding
        // PendingPing.
        let mut parked: Vec<NodeId> = Vec::new();
        while parked.len() < BUCKET_SIZE {
            let id = id_in_bucket(&t.self_id, target_bucket, &mut cursor);
            if active.contains(&id) || parked.contains(&id) {
                continue;
            }
            parked.push(id);
            match t.insert(desc(id)) {
                InsertOutcome::PendingPing(_) => {}
                other => {
                    panic!("expected PendingPing while filling candidates, got {other:?}")
                }
            }
        }

        // Now both vecs are full — the next *fresh* id in the same
        // bucket must be Discarded.
        loop {
            let id = id_in_bucket(&t.self_id, target_bucket, &mut cursor);
            if active.contains(&id) || parked.contains(&id) {
                continue;
            }
            assert_eq!(t.insert(desc(id)), InsertOutcome::Discarded);
            break;
        }
    }

    #[test]
    fn insert_self_id_returns_is_self() {
        let mut t = fresh_table(7);
        let self_desc = desc(t.self_id);
        assert_eq!(t.insert(self_desc), InsertOutcome::IsSelf);
        assert_eq!(t.total_known(), 0);
        // Routing-table state is genuinely untouched: `total_known` only
        // counts active entries, so also confirm nothing was parked in
        // any bucket's `candidates` cache.
        for bucket in t.buckets.iter() {
            assert!(bucket.candidates.is_empty());
        }
    }

    // -------- find_closest ---------------------------------------------

    #[test]
    fn find_closest_returns_xor_sorted_descriptors() {
        let mut t = fresh_table(0);
        // Insert 10 distinct peers. With BLAKE3-derived ids they spread
        // across buckets randomly, exactly the case `find_closest` is
        // built for.
        let mut inserted: Vec<NodeId> = Vec::new();
        for n in 1..=10u8 {
            let id = id_n(n);
            // Defensive: if a seed collides with self_id (statistically
            // impossible) we'd silently lose a peer.
            assert_ne!(id, t.self_id);
            inserted.push(id);
            assert_eq!(t.insert(desc(id)), InsertOutcome::Inserted);
        }

        let target = id_n(0xAA);
        let got = t.find_closest(&target, 5);
        assert_eq!(got.len(), 5);

        // Compute the brute-force expected order and confirm.
        let mut expected: Vec<(_, NodeId)> = inserted
            .iter()
            .copied()
            .map(|id| {
                let dist = xor_bytes(id.as_bytes(), target.as_bytes());
                (dist, id)
            })
            .collect();
        expected.sort_by_key(|a| a.0);
        let expected_ids: Vec<NodeId> = expected.into_iter().take(5).map(|(_, id)| id).collect();
        let got_ids: Vec<NodeId> = got.iter().map(|d| d.id).collect();
        assert_eq!(got_ids, expected_ids);
    }

    #[test]
    fn find_closest_empty_table_returns_empty() {
        let t = fresh_table(0);
        let got = t.find_closest(&id_n(99), 5);
        assert!(got.is_empty());
    }

    #[test]
    fn find_closest_count_zero_returns_empty() {
        let mut t = fresh_table(0);
        t.insert(desc(id_n(1)));
        assert!(t.find_closest(&id_n(2), 0).is_empty());
    }

    // -------- ping_failed / ping_succeeded -----------------------------

    #[test]
    fn ping_failed_evicts_after_three_failures() {
        let mut t = fresh_table(0);
        let p = id_n(1);
        t.insert(desc(p));
        assert_eq!(t.total_known(), 1);

        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Evicted);

        assert_eq!(t.total_known(), 0);
        // A subsequent ping_failed against the now-removed peer is
        // reported as Unknown (no panic, no underflow).
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Unknown);
    }

    #[test]
    fn ping_failed_evicts_and_promotes_candidate() {
        let mut t = fresh_table(0);
        let mut cursor = 0u32;
        let target_bucket = 255;

        // Fill the bucket.
        let active = fill_bucket(&mut t, target_bucket, &mut cursor);

        // Add one more peer in the same bucket → goes to candidates.
        let candidate_id = loop {
            let id = id_in_bucket(&t.self_id, target_bucket, &mut cursor);
            if !active.contains(&id) {
                break id;
            }
        };
        assert!(matches!(
            t.insert(desc(candidate_id)),
            InsertOutcome::PendingPing(_)
        ));

        // The LRU (active[0]) failing 3 times should evict it AND
        // promote `candidate_id` into the bucket.
        let lru = active[0];
        assert_eq!(t.ping_failed(&lru), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&lru), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&lru), PingFailedOutcome::EvictedAndPromoted);

        assert_eq!(t.total_known(), BUCKET_SIZE);
        // The promoted entry should now be discoverable in the bucket.
        let bucket = &t.buckets[target_bucket];
        assert!(bucket.entries.iter().any(|e| e.id == candidate_id));
        assert!(!bucket.entries.iter().any(|e| e.id == lru));
        assert!(bucket.candidates.is_empty());
    }

    #[test]
    fn ping_succeeded_resets_failure_counter() {
        let mut t = fresh_table(0);
        let p = id_n(1);
        t.insert(desc(p));

        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        // Reset before the third failure.
        assert!(t.ping_succeeded(&p, 100));
        // Two more failures should NOT evict — counter went back to 0.
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        assert_eq!(t.ping_failed(&p), PingFailedOutcome::Continued);
        assert_eq!(t.total_known(), 1);
    }

    #[test]
    fn ping_succeeded_updates_ema() {
        let mut t = fresh_table(0);
        let p = id_n(1);
        t.insert(desc(p));

        // First sample seeds the EMA exactly.
        assert!(t.ping_succeeded(&p, 100));
        let bucket_idx = bucket_for(&t.self_id, &p).unwrap();
        let ema_after_first = t.buckets[bucket_idx]
            .entries
            .iter()
            .find(|e| e.id == p)
            .unwrap()
            .rtt_ema_ms;
        assert_eq!(ema_after_first, Some(100));

        // Second sample at 200: ema = (100 * 7 + 200) / 8 = 112.
        assert!(t.ping_succeeded(&p, 200));
        let ema_after_second = t.buckets[bucket_idx]
            .entries
            .iter()
            .find(|e| e.id == p)
            .unwrap()
            .rtt_ema_ms;
        assert_eq!(ema_after_second, Some(112));
    }

    #[test]
    fn ping_succeeded_unknown_peer_returns_false() {
        let mut t = fresh_table(0);
        // Nothing inserted; the peer is unknown.
        assert!(!t.ping_succeeded(&id_n(1), 50));
    }

    // -------- refresh policy -------------------------------------------

    #[test]
    fn buckets_needing_refresh_returns_stale_indices() {
        let t = fresh_table(0);
        // Fresh table — no buckets stale "now".
        assert!(t.buckets_needing_refresh(Instant::now()).is_empty());

        // Use a future "now" past BUCKET_REFRESH_MS to simulate elapsed
        // time without sleeping the test thread.
        let far_future = Instant::now()
            + Duration::from_millis(super::super::config::BUCKET_REFRESH_MS + 1_000);
        let stale = t.buckets_needing_refresh(far_future);
        assert_eq!(stale.len(), BUCKETS);
    }

}
