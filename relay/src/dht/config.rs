//! DHT-wide constants and the operator-tunable [`DhtConfig`].
//!
//! Constants are baked in here as `pub const`. Anything that should be
//! operator-tunable is on [`DhtConfig`] in the relay's TOML; the rest is
//! intentionally hard-coded so all relays in the network agree on protocol
//! parameters without per-deployment drift.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Replication & lookup parameters
// ---------------------------------------------------------------------------

/// Replication factor — number of replicas per `(user_ipk → PresenceRecord)`.
pub const K: usize = 3;

/// Lookup parallelism — concurrent `FindNode`/`FindValue` RPCs in flight
/// per iterative walk.
pub const ALPHA: usize = 3;

/// Per-bucket capacity (k-bucket size).
pub const BUCKET_SIZE: usize = 16;

/// Number of k-buckets in the routing table — one per leading-zero-bit class
/// of a 256-bit `NodeId`.
pub const BUCKETS: usize = 256;

// ---------------------------------------------------------------------------
// Lookup timing
// ---------------------------------------------------------------------------

/// Per-hop hedged-request delay. After this elapses with no reply we issue
/// the next candidate query in parallel.
pub const LOOKUP_HEDGE_MS: u64 = 150;

/// Per-RPC timeout ceiling.
pub const LOOKUP_RPC_TIMEOUT_MS: u64 = 1500;

/// Maximum hops per iterative lookup.
pub const LOOKUP_MAX_HOPS: u32 = 8;

/// Ceiling on an iterative walk's candidate shortlist. Peers only ever
/// contribute descriptors the walk might dial, so the pool is trimmed to
/// the closest `MAX_LOOKUP_CANDIDATES` after each hop's merge.
pub const MAX_LOOKUP_CANDIDATES: usize = 64;

// ---------------------------------------------------------------------------
// Presence record lifetimes
// ---------------------------------------------------------------------------

/// Presence record TTL — replicas reject records older than this past
/// `not_after` (10 minutes).
pub const PRESENCE_TTL_MS: u64 = 600_000;

// ---------------------------------------------------------------------------
// Merkle / anti-entropy
// ---------------------------------------------------------------------------

/// Top-level slice prefix size in bits — slices the keyspace into
/// `2^MERKLE_SLICE_BITS = 256` equal regions.
pub const MERKLE_SLICE_BITS: u32 = 8;

/// Leaf granularity in bits — each Merkle leaf covers `2^MERKLE_LEAF_BITS`
/// keys within its slice.
pub const MERKLE_LEAF_BITS: u32 = 16;

/// Branching factor of the per-slice trie (4 bits per level).
pub const MERKLE_FANOUT: usize = 16;

/// Anti-entropy pull cadence — how often we pull a `MerkleSummary` from a
/// random peer in our routing table.
pub const ANTI_ENTROPY_INTERVAL_MS: u64 = 30_000;

/// Bucket-refresh staleness threshold (1 hour).
pub const BUCKET_REFRESH_MS: u64 = 3_600_000;

// ---------------------------------------------------------------------------
// Quorum parameters
// ---------------------------------------------------------------------------

/// Strict-quorum threshold for the iterative `lookup_value` walk. A
/// `Found` reply is only honoured if at least `LOOKUP_QUORUM` peers
/// (out of the K-closest contacted) returned an *agreeing* `Found` —
/// agreement defined as "same `(generation, relay_id)` pair". Otherwise
/// the iteration treats the lone `Found` as suspect and returns
/// `NotPresent`.
///
/// **Tradeoff:** a record that was just published (and only stored on its
/// first replica so far — natural during the ~30 s anti-entropy window)
/// appears as a 1-hit, K-1 NotPresent situation here, so a strict quorum
/// returns false-NotPresent for up to one anti-entropy round. The
/// publishing relay is the canonical home; any cache that lives there
/// bridges that window.
///
/// One-line tunable so the quorum threshold can be loosened in test
/// clusters without a code edit ripple.
pub const LOOKUP_QUORUM: usize = 2;

// ---------------------------------------------------------------------------
// RPC bounds
// ---------------------------------------------------------------------------

/// Maximum entries returned in a single `FetchRecord` request/response.
pub const FETCH_RECORD_MAX: usize = 64;

/// Maximum entries packed into a single `MerkleDiff::Leaves` response.
pub const MERKLE_DIFF_LEAVES_MAX: usize = 64;

/// Maximum depth of a `MerkleDiff::path` (radix-16 over 16-bit leaf space).
pub const MERKLE_DIFF_PATH_MAX: usize = 4;

/// Maximum concurrent `FetchRecord` RPCs a fresh-joiner issues during
/// cold-join, to avoid DoSing neighbours.
pub const FETCH_RECORD_CONCURRENCY: usize = 8;

// ---------------------------------------------------------------------------
// Sticky-home Forward fan-out
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
/// 1500 ms aligns with the per-RPC ceiling already enforced by
/// `lookup`/`publish` so all parallel-fan-out paths share one
/// timeout-budget contract.
pub const FORWARD_TIMEOUT_MS: u64 = 1500;

/// Minimum number of "Delivered or Stored" outcomes from the K homes
/// required for the sender relay to ack the originating client with
/// [`common::proto::client_rel::DispatchAckP::Forwarded`].
///
/// Set to 2 (= 2-of-3 with `K = 3`), mirroring [`LOOKUP_QUORUM`]: the
/// same threshold ensures cross-checked reads on the recipient side have
/// at least the same redundancy as cross-checked writes on the sender
/// side. Below this threshold the sender falls back to local queueing.
pub const FORWARD_K_MIN: usize = 2;

// ---------------------------------------------------------------------------
// Sticky-home QueueFetch fan-out
// ---------------------------------------------------------------------------

/// Total wall-clock budget for the K-1 (or K) parallel `QueueFetch`
/// RPCs the recipient relay issues to home relays when this relay is
/// not in the user's K-closest set.
///
/// Sized 2× [`FORWARD_TIMEOUT_MS`] (3000 ms vs 1500 ms) to tolerate
/// parallel connections to every home. Each drain fetches one bounded
/// batch per home; acknowledged reconnects advance through longer queues.
///
/// On timeout, the recipient relay treats in-flight homes as "no
/// response" (best-effort) and still delivers whatever pages completed.
/// The user can retry the drain — the homes won't have deleted anything
/// until a `QueueFetchAck` lands.
pub const QUEUE_FETCH_TIMEOUT_MS: u64 = 3000;


// ---------------------------------------------------------------------------
// Sticky-home K-set drift migration
// ---------------------------------------------------------------------------

/// Defensive cap on the number of `cf_dht_queue` entries a single
/// `evict_expired` sweep will migrate when this relay realises it has
/// drifted out of a recipient's K-closest set.
///
/// The migration runs lazily on every periodic `evict_expired` sweep. A
/// sweep over a fully-loaded disk (millions of `cf_dht_queue` entries)
/// spent on synchronous per-entry K-closest lookups + outbound `Forward`
/// RPCs would stall the scheduler and hog network bandwidth. Capping at
/// 256 keeps the per-sweep CPU and outbound-RPC fan-out bounded; the next
/// sweep (after `EVICT_INTERVAL_MS = 60s`) handles the remainder.
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
pub const MAX_MIGRATE_PER_SWEEP: usize = 256;

/// Maximum number of in-flight `forward_to_homes` migration tasks
/// the periodic scheduler will run in parallel during one drift sweep.
/// Bounds the outbound RPC fan-out so a sweep can complete even when
/// every candidate's new K-closest set is unhealthy: each migration
/// opens up to K=3 outbound `Forward` RPCs (1500 ms `FORWARD_TIMEOUT_MS`
/// ceiling each), so a single migration can hold up to 3 outbound
/// bi-streams worst-case. Capping concurrent migrations at 8 → ≤24
/// simultaneous outbound `Forward` streams, well inside any reasonable
/// per-peer connection limit.
///
/// Same magnitude as [`FETCH_RECORD_CONCURRENCY`] (= 8) — both are
/// post-bootstrap I/O fan-out caps in the same regime.
///
/// **Sweep wall-clock budget**: a fully-saturated
/// `MAX_MIGRATE_PER_SWEEP = 256` candidates serialised across 8
/// concurrent slots = 32 sequential mini-batches; each mini-batch
/// completes in ≤`FORWARD_TIMEOUT_MS` (1500 ms) → upper bound ~48 s
/// per sweep, comfortably inside the 60 s `EVICT_INTERVAL_MS`. A
/// healthy network completes each migration in ~50 ms (one RTT per
/// home), so the typical sweep finishes well under 2 s.
pub const MAX_CONCURRENT_MIGRATIONS: usize = 8;

// ---------------------------------------------------------------------------
// Inbound-RPC rate limits (DoS hardening)
// ---------------------------------------------------------------------------
//
// `governor::Quota` is configured `per_second(rate).allow_burst(burst)`.
// Each RPC-class limiter is keyed on the *requester's* NodeId; the
// global limiter below is unkeyed and bounds the aggregate, because a
// NodeId costs one Ed25519 keygen to mint and the per-peer quotas alone
// therefore bound nothing in aggregate.

/// Cheap RPCs (`FindNode`): routing-table reads, no signature
/// verification and no disk I/O. Quota absorbs iterative-lookup batches
/// with hedged retries — a K=3 walk with α=3 parallelism and 8 hops is
/// ~24 RPCs, roughly 40× under one second's allowance.
pub const RATE_LIMIT_CHEAP_PER_SEC: u32 = 1_000;
pub const RATE_LIMIT_CHEAP_BURST: u32 = 500;

/// Verify-and-write RPCs — the sticky-home family (`Forward`,
/// `QueueFetch`, `QueueFetchAck`, the presence/live RPCs) and the MLS
/// KeyPackage family. Each does at least one Ed25519 verify (~100 µs)
/// plus a synced fjall write or a bounded prefix scan. At 200/s the
/// verify load is ~2% of one core.
pub const RATE_LIMIT_EXPENSIVE_PER_SEC: u32 = 200;
pub const RATE_LIMIT_EXPENSIVE_BURST: u32 = 100;

/// Bulk RPCs — the MLS Welcome family, whose `welcome_blob` reaches
/// `MAX_WELCOME_BYTES` (256 KiB) and whose fetch returns up to
/// `MAX_WELCOMES_PER_RECIPIENT` rows per request. Sized between CHEAP
/// and EXPENSIVE: the crypto cost is lower than a per-record verify but
/// the bytes-per-RPC is the highest in the DHT family.
pub const RATE_LIMIT_BULK_PER_SEC: u32 = 500;
pub const RATE_LIMIT_BULK_BURST: u32 = 250;

/// Aggregate inbound-RPC ceiling across every peer and class. Sits an
/// order of magnitude above one peer's CHEAP quota so a healthy mesh
/// never touches it, while capping what an attacker can extract by
/// minting fresh NodeIds.
pub const RATE_LIMIT_GLOBAL_PER_SEC: u32 = 10_000;
pub const RATE_LIMIT_GLOBAL_BURST: u32 = 5_000;

// ---------------------------------------------------------------------------
// Operator-tunable config (TOML-deserialisable)
// ---------------------------------------------------------------------------

/// Operator-tunable subset of the DHT parameters.
///
/// Only knobs that genuinely vary per-deployment live here — everything
/// else is a hard-coded `pub const` above. Protocol parameters stay
/// hard-coded because all relays in the network must agree; TOML drift
/// would silently break routing.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DhtConfig {
    /// Master kill-switch. When `false`, the relay constructs no [`Dht`] and
    /// every code path that would touch one falls through to the
    /// pre-DHT behaviour. Default is `false`.
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

    /// Permit dialling peers whose advertised address is loopback or in
    /// an RFC1918/ULA range. Off in production, where a peer-supplied
    /// address naming an internal host is an SSRF primitive; on for
    /// single-host test clusters.
    #[serde(default)]
    pub allow_local_peer_addrs: bool,
}

impl DhtConfig {
    /// Effective bucket size: the operator override if set, otherwise the
    /// canonical [`BUCKET_SIZE`].
    pub fn bucket_size(&self) -> usize {
        self.bucket_size.unwrap_or(BUCKET_SIZE)
    }
}
