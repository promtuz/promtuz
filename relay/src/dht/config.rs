//! DHT-wide constants and the operator-tunable [`DhtConfig`].
//!
//! Constants are baked in here as `pub const` (§0 of `misc/specs/DHT.md`).
//! Anything that should be operator-tunable is on [`DhtConfig`] in the
//! relay's TOML; the rest is intentionally hard-coded so all relays in the
//! network agree on protocol parameters without per-deployment drift.
//!
//! design-doc: §0 (constants table), §11.8 (default `enabled = false`)

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Replication & lookup parameters
// ---------------------------------------------------------------------------

/// Replication factor — number of replicas per `(user_ipk → PresenceRecord)`.
///
/// design-doc: §0 (`k = 3`)
pub const K: usize = 3;

/// Lookup parallelism — concurrent `FindNode`/`FindValue` RPCs in flight
/// per iterative walk.
///
/// design-doc: §0 (`α = 3`)
pub const ALPHA: usize = 3;

/// Per-bucket capacity (k-bucket size).
///
/// design-doc: §0 (`B = 16`)
pub const BUCKET_SIZE: usize = 16;

/// Number of k-buckets in the routing table — one per leading-zero-bit class
/// of a 256-bit `NodeId`.
///
/// design-doc: §0 (`BUCKETS = 256`)
pub const BUCKETS: usize = 256;

// ---------------------------------------------------------------------------
// Lookup timing
// ---------------------------------------------------------------------------

/// Per-hop hedged-request delay. After this elapses with no reply we issue
/// the next candidate query in parallel.
///
/// design-doc: §0 (`LOOKUP_HEDGE_MS = 150`)
pub const LOOKUP_HEDGE_MS: u64 = 150;

/// Per-RPC timeout ceiling.
///
/// design-doc: §0 (`LOOKUP_RPC_TIMEOUT_MS = 1500`)
pub const LOOKUP_RPC_TIMEOUT_MS: u64 = 1500;

/// Maximum hops per iterative lookup.
///
/// design-doc: §0 (`LOOKUP_MAX_HOPS = 8`)
pub const LOOKUP_MAX_HOPS: u32 = 8;

// ---------------------------------------------------------------------------
// Presence record lifetimes
// ---------------------------------------------------------------------------

/// Presence record TTL — replicas reject records older than this past
/// `not_after`.
///
/// design-doc: §0 (`PRESENCE_TTL_MS = 600_000`, 10 minutes)
pub const PRESENCE_TTL_MS: u64 = 600_000;

/// Republish cadence — owning relay refreshes every record this often.
/// Must be `<<` `PRESENCE_TTL_MS`.
///
/// design-doc: §0 (`PRESENCE_REPUBLISH_MS = 240_000`, 4 minutes)
pub const PRESENCE_REPUBLISH_MS: u64 = 240_000;

/// Future-clock tolerance on signed `not_before`.
///
/// design-doc: §0 (`PRESENCE_MAX_FUTURE_SKEW_MS = 60_000`, 1 minute)
pub const PRESENCE_MAX_FUTURE_SKEW_MS: u64 = 60_000;

// ---------------------------------------------------------------------------
// Merkle / anti-entropy
// ---------------------------------------------------------------------------

/// Top-level slice prefix size in bits — slices the keyspace into
/// `2^MERKLE_SLICE_BITS = 256` equal regions.
///
/// design-doc: §0 (`MERKLE_SLICE_BITS = 8`)
pub const MERKLE_SLICE_BITS: u32 = 8;

/// Leaf granularity in bits — each Merkle leaf covers `2^MERKLE_LEAF_BITS`
/// keys within its slice.
///
/// design-doc: §0 (`MERKLE_LEAF_BITS = 16`)
pub const MERKLE_LEAF_BITS: u32 = 16;

/// Branching factor of the per-slice trie.
///
/// design-doc: §0 (`MERKLE_FANOUT = 16`, 4 bits per level)
pub const MERKLE_FANOUT: usize = 16;

/// Optional per-slice bloom filter bit width (operator visibility only).
///
/// design-doc: §0 (`BLOOM_BITS = 65_536`)
pub const BLOOM_BITS: u32 = 65_536;

/// Optional per-slice bloom filter hash count.
///
/// design-doc: §0 (`BLOOM_HASHES = 6`)
pub const BLOOM_HASHES: u32 = 6;

/// Anti-entropy pull cadence — how often we pull a `MerkleSummary` from a
/// random peer in our routing table.
///
/// design-doc: §0 (`ANTI_ENTROPY_INTERVAL_MS = 30_000`)
pub const ANTI_ENTROPY_INTERVAL_MS: u64 = 30_000;

/// Bucket-refresh staleness threshold.
///
/// design-doc: §0 (`BUCKET_REFRESH_MS = 3_600_000`, 1 hour)
pub const BUCKET_REFRESH_MS: u64 = 3_600_000;

// ---------------------------------------------------------------------------
// Wire-signature domain
// ---------------------------------------------------------------------------

/// Base domain prefix used to construct every DHT wire-signature transcript
/// (§1.1.1). Sub-domains (`-roam-v1`, `-presence-v1`, …) are appended at
/// signing time so a captured signature for one packet kind cannot be
/// replayed as another.
///
/// design-doc: §0 (`DHT_DOMAIN_PREFIX = "promtuz-dht-v1"`)
pub const DHT_DOMAIN_PREFIX: &[u8] = b"promtuz-dht-v1";

// ---------------------------------------------------------------------------
// Quorum and lookup-cache parameters
// ---------------------------------------------------------------------------

/// Minimum number of agreeing `FindValue` replies required to accept a
/// first-time lookup answer.
///
/// design-doc: §4.4 (cross-checking, `min_quorum = 2`)
pub const MIN_QUORUM: usize = 2;

/// Strict-quorum threshold for the iterative `lookup_value` walk. A
/// `Found` reply is only honoured if at least `LOOKUP_QUORUM` peers
/// (out of the K-closest contacted) returned an *agreeing* `Found` —
/// agreement defined as "same `(generation, relay_id)` pair" per §4.4.
/// Otherwise the iteration treats the lone `Found` as suspect and
/// returns `NotPresent`.
///
/// **Tradeoff documented inline:** a record that was just published
/// (and only stored on its first replica so far — natural during the
/// ~30 s anti-entropy window) appears as a 1-hit, K-1 NotPresent
/// situation here, so a strict quorum returns false-NotPresent for up
/// to one anti-entropy round. The publishing relay is the canonical
/// home; any cache that lives there bridges that window. See §6.3.
///
/// One-line tunable so the quorum threshold can be loosened in test
/// clusters without a code edit ripple.
///
/// design-doc: §4.4 (Sybil/eclipse mitigation — quorum read).
pub const LOOKUP_QUORUM: usize = 2;

/// Cap on the number of entries the lookup-result cache holds.
///
/// design-doc: §4.4 (cache for repeat recipients)
pub const LOOKUP_CACHE_CAP: usize = 4_096;

// ---------------------------------------------------------------------------
// RPC bounds (§2.6)
// ---------------------------------------------------------------------------

/// Maximum entries returned in a single `FetchRecord` request/response.
///
/// design-doc: §2.6 length bounds
pub const FETCH_RECORD_MAX: usize = 64;

/// Maximum entries packed into a single `MerkleDiff::Leaves` response.
///
/// design-doc: §2.6 length bounds
pub const MERKLE_DIFF_LEAVES_MAX: usize = 64;

/// Maximum depth of a `MerkleDiff::path` (radix-16 over 16-bit leaf space).
///
/// design-doc: §2.6 length bounds
pub const MERKLE_DIFF_PATH_MAX: usize = 4;

/// Maximum concurrent `FetchRecord` RPCs a fresh-joiner issues during
/// cold-join, to avoid DoSing neighbours.
///
/// design-doc: §7.3 (`Rate-limit: J caps concurrent FetchRecord to 8`)
pub const FETCH_RECORD_CONCURRENCY: usize = 8;

// ---------------------------------------------------------------------------
// Sticky-home Forward fan-out (phase 2b)
// ---------------------------------------------------------------------------

/// Total wall-clock budget for the K parallel `Forward` RPCs the sender
/// relay issues during sticky-home fan-out.
///
/// Sized to match [`LOOKUP_RPC_TIMEOUT_MS`] (1500 ms): each individual
/// `Forward` is a single bi-stream that opens, writes a small request,
/// reads a small response, and finishes — the same network round-trip
/// shape as a `Store`. A K=3 fan-out completes well inside this window
/// in steady state; the cap is a fail-safe so a wedged peer can't stall
/// a sender's `Dispatch` ack indefinitely. On timeout the sender treats
/// in-flight homes as "no response" and falls back to the local queue.
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.2 step 5 (the K_MIN
/// quorum window). 1500 ms aligns with the per-RPC ceiling already
/// enforced by `lookup`/`publish` so all parallel-fan-out paths share
/// one timeout-budget contract.
pub const FORWARD_TIMEOUT_MS: u64 = 1500;

/// Minimum number of "Delivered or Stored" outcomes from the K homes
/// required for the sender relay to ack the originating client with
/// [`common::proto::client_rel::DispatchAckP::Forwarded`].
///
/// Set to 2 (= 2-of-3 with `K = 3`), mirroring [`publish::K_MIN`] and
/// [`LOOKUP_QUORUM`]: the sticky-home spec §4.2 step 5 explicitly calls
/// out the same threshold so cross-checked reads on the recipient side
/// have at least the same redundancy as cross-checked writes on the
/// sender side. Below this threshold the sender falls back to local
/// queueing per §4.2 step 7 (and §4.5 "Forwarding fails (<2 of K
/// accept)" row).
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.2 step 5, §7
/// question 1 (recommended: K_MIN = 2 for K = 3).
pub const FORWARD_K_MIN: usize = 2;

// ---------------------------------------------------------------------------
// Sticky-home QueueFetch fan-out (phase 2c)
// ---------------------------------------------------------------------------

/// Total wall-clock budget for the K-1 (or K) parallel `QueueFetch`
/// RPCs the recipient relay issues to home relays when this relay is
/// not in the user's K-closest set (`STICKY_HOME_RELAY.md` §4.3 step 3).
///
/// Sized 2× [`FORWARD_TIMEOUT_MS`] (3000 ms vs 1500 ms) because a
/// single `QueueFetch` can page over multiple round-trips when the
/// home's queue exceeds [`MAX_FETCH_QUEUE_BATCH`] entries — each page
/// is one bi-stream + verify + iterator + serialize at the home, so a
/// 1024-entry user can need 16 pages × 3 homes worst case. The 3 s
/// cap is the absolute fail-safe; in steady state a typical drain is
/// 1-2 pages and completes well inside [`FORWARD_TIMEOUT_MS`].
///
/// On timeout, the recipient relay treats in-flight homes as "no
/// response" (best-effort) and still delivers whatever pages
/// completed. The user can retry the drain — the homes won't have
/// deleted anything until a `QueueFetchAck` lands (phase 2d).
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.3 step 3 (the K
/// home-fetch fan-out window).
pub const QUEUE_FETCH_TIMEOUT_MS: u64 = 3000;

/// Defensive upper bound on the number of pages the recipient relay
/// will request from a single home in one `fetch_remote_queues` call.
/// Each page is one round-trip carrying up to
/// [`crate::storage::MAX_QUEUED_PER_RECIPIENT`] / 16 (=
/// `MAX_FETCH_QUEUE_BATCH = 64`) dispatches.
///
/// The legitimate maximum is `ceil(MAX_QUEUED_PER_RECIPIENT /
/// MAX_FETCH_QUEUE_BATCH) = ceil(1024 / 64) = 16`. We cap at **10**
/// because a misbehaving home that never returns `exhausted = true`
/// would otherwise spin forever — `MAX_FETCH_QUEUE_BATCH * 10 = 640`
/// dispatches is well past any plausible per-user backlog and far
/// below the theoretical 1024-entry cap. A user at the cap will see
/// 10 pages × 64 = 640 messages drained on first reconnect; the
/// remainder lingers at the home until natural TTL expiry and
/// becomes ineligible at next reconnect.
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.3 — paging
/// implied by `QueueFetchResp::exhausted`. The bound is a
/// phase-2c-introduced safety rail and is not on the wire.
pub const MAX_QUEUE_FETCH_PAGES: usize = 10;

// ---------------------------------------------------------------------------
// Sticky-home K-set drift migration (phase 2d)
// ---------------------------------------------------------------------------

/// Defensive cap on the number of `cf_dht_queue` entries a single
/// `evict_expired` sweep will migrate when this relay realises it has
/// drifted out of a recipient's K-closest set.
///
/// Per `STICKY_HOME_RELAY.md` §4.4 / §7.2 the migration runs lazily on
/// every periodic `evict_expired` sweep. A sweep over a fully-loaded
/// disk (millions of `cf_dht_queue` entries) spent on synchronous
/// per-entry K-closest lookups + outbound `Forward` RPCs would stall
/// the scheduler and hog network bandwidth. Capping at 256 keeps the
/// per-sweep CPU and outbound-RPC fan-out bounded; the next sweep
/// (after `EVICT_INTERVAL_MS = 60s`) handles the remainder.
///
/// The cap is intentionally per-sweep rather than per-recipient —
/// even a single recipient with 1024 queued messages (the
/// per-recipient cap) is well under the 256 budget *if* it's the only
/// migration candidate. A relay that drifted out of K for many
/// recipients simultaneously gets the spread treatment over multiple
/// sweeps, which is the correct shape under churn.
///
/// 256 was chosen to balance:
/// - sweep wall-clock budget (one outbound bi-stream per migrated
///   message; 256 × ~5 ms = ~1 s worst case, comfortably inside
///   the 60 s sweep interval),
/// - storage drainage rate (a permanently-displaced relay is
///   re-emptied within ~1 hour at the steady rate), and
/// - the existing `FETCH_RECORD_CONCURRENCY = 8` cold-join cap
///   pattern (this is the post-bootstrap analogue).
///
/// design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.4 (drift handling),
/// §7.2 (lazy on `evict_expired` sweep).
pub const MAX_MIGRATE_PER_SWEEP: usize = 256;

// ---------------------------------------------------------------------------
// Per-peer inbound-RPC rate limits (§8.4 / §8.7 DoS hardening)
// ---------------------------------------------------------------------------
//
// `governor::Quota` is configured `per_second(rate).allow_burst(burst)`.
// Each RPC-class limiter is keyed on the *requester's* NodeId so a
// single misbehaving peer can be sanctioned without affecting others.
//
// Quota values were picked to leave the legitimate anti-entropy
// scheduler firing every `ANTI_ENTROPY_INTERVAL_MS = 30_000` ms (§0)
// well below their thresholds:
//   - Anti-entropy: ~1 MerkleSummary + ~5 MerkleDiffs per round → ~1
//     RPC/3s per pair, < CHEAP quota by 100x.
//   - Cold-join (§7.3): worst-case `FETCH_RECORD_CONCURRENCY = 8`
//     parallel FetchRecords spread over 1 s. The BULK quota allows 50
//     in 1 s → 6x headroom.
//   - Publish (§5.2): 1 Store per replica per record, K = 3 replicas.
//     Steady state at 100 publishes/s/relay = 100 Stores/s into K
//     replicas distributed → ~33/s into the busiest one. The EXPENSIVE
//     quota of 20/s is below this; in steady-state high load the
//     quota would trip. Tradeoff is acceptable for v1: a publishing
//     relay sees `RateLimited` from a single overloaded replica and
//     re-tries via the §5.2 escalation path. Phase 2 telemetry
//     (§11.4) will revisit.

/// Cheap RPCs (Ping, FindNode, FindValue, MerkleSummary, MerkleDiff):
/// no on-disk crypto verification and only routing-table reads. Quota
/// is generous enough to absorb iterative-lookup batches that include
/// hedged retries (§4.1). Sustained 100 req/s with bursts of 50 means
/// a steady-state of 50 req/s with one in-flight batch of 50 spikes
/// not getting flagged.
pub const RATE_LIMIT_CHEAP_PER_SEC: u32 = 100;
pub const RATE_LIMIT_CHEAP_BURST: u32 = 50;

/// Expensive verify RPCs (Store, Tombstone). Each triggers Ed25519
/// signature verification and a synced RocksDB write. Tighter quota
/// than CHEAP because the per-op cost is ~100 µs of crypto + an fsync;
/// at 20/s sustained the verify load is 0.2% of one CPU.
pub const RATE_LIMIT_EXPENSIVE_PER_SEC: u32 = 20;
pub const RATE_LIMIT_EXPENSIVE_BURST: u32 = 10;

/// Bulk RPCs (FetchRecord). Each request is bounded by
/// [`FETCH_RECORD_MAX = 64`] entries; sustained 50 req/s × 64 ipks/req
/// = 3200 record reads/s, which is well within RocksDB's hot-path
/// ceiling and matches the §7.3 cold-join concurrency budget.
pub const RATE_LIMIT_BULK_PER_SEC: u32 = 50;
pub const RATE_LIMIT_BULK_BURST: u32 = 25;

// ---------------------------------------------------------------------------
// Operator-tunable config (TOML-deserialisable)
// ---------------------------------------------------------------------------

/// Operator-tunable subset of the DHT parameters.
///
/// Only knobs that genuinely vary per-deployment live here — everything
/// else is a hard-coded `pub const` above (§0). The full design-doc rationale
/// for keeping protocol parameters out of operator config is "all relays in
/// the network must agree" — TOML drift would silently break routing.
///
/// design-doc: §10 (Phase 1 feature-flag), §11.8 (default `enabled = false`)
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DhtConfig {
    /// Master kill-switch. When `false`, the relay constructs no [`Dht`] and
    /// every code path that would touch one falls through to the
    /// pre-DHT behaviour.
    ///
    /// design-doc: §11.8 — Phase 1 default is `false`; flip to `true` only
    /// inside test deployments until Phase 3 cutover.
    ///
    /// [`Dht`]: crate::dht::Dht
    #[serde(default)]
    pub enabled: bool,

    /// Override of [`BUCKET_SIZE`] for testing. `None` means "use the
    /// constant" (the canonical production value).
    ///
    /// Allowing this to vary lets a test cluster run with a smaller bucket
    /// size to force eviction-path coverage with a tractable peer count.
    /// Production deployments should leave it unset.
    #[serde(default)]
    pub bucket_size: Option<usize>,
}

impl DhtConfig {
    /// Effective bucket size: the operator override if set, otherwise the
    /// canonical [`BUCKET_SIZE`].
    pub fn bucket_size(&self) -> usize {
        self.bucket_size.unwrap_or(BUCKET_SIZE)
    }
}
