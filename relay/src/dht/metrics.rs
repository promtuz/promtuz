//! Per-relay DHT operation counters.
//!
//! Plain-and-obvious `AtomicU64`s, one per kind of event listed in §11.4 /
//! the dispatch's required-counters list. No histograms — just counts. A
//! later pass can wrap these in a Prometheus-style exporter.
//!
//! design-doc: §9.1 (`metrics.rs`), §11.4 (need-instrumentation).

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Aggregate counters covering every DHT operation a relay can observe.
///
/// All increments are `Relaxed`: counters are read for monitoring/debug
/// and don't synchronise other state. The cost of `Relaxed` `fetch_add`
/// is a single atomic op (no fence), trivial relative to any RPC.
#[derive(Debug, Default)]
pub struct Metrics {
    // --- iterative lookups (§4) ---
    pub lookups_started:   AtomicU64,
    pub lookups_succeeded: AtomicU64,
    pub lookups_failed:    AtomicU64,

    // --- store path (§5) ---
    pub stores_received: AtomicU64,
    pub stores_accepted: AtomicU64,
    pub stores_rejected: AtomicU64,

    // --- inbound RPCs (§2.4) ---
    pub find_node_rpcs:  AtomicU64,
    pub find_value_rpcs: AtomicU64,
    pub pings_sent:      AtomicU64,
    pub pings_received:  AtomicU64,

    // --- anti-entropy (§6) ---
    pub merkle_summaries_sent:     AtomicU64,
    pub merkle_summaries_received: AtomicU64,
    pub merkle_diffs_sent:         AtomicU64,
    pub merkle_diffs_received:     AtomicU64,

    // --- routing-table churn (§3.3) ---
    pub bucket_evictions: AtomicU64,

    // --- peer connection lifecycle (§7.1) ---
    pub peer_conns_opened: AtomicU64,
    pub peer_conns_closed: AtomicU64,

    // --- DoS hardening ---
    /// Per-peer rate limit was tripped on an inbound RPC. Bumped once
    /// per rejected stream — multiple denied calls within one
    /// connection still bump exactly once before the close.
    pub rate_limit_rejections: AtomicU64,

    /// Inbound `peer/1` connection rejected because the post-handshake
    /// TLS-pubkey extraction failed (cert chain absent, malformed
    /// SPKI, self-sig invalid, or `BLAKE3(spki) != claimed_node_id`).
    /// Bumped on the dial-side path in `lookup::connect_to_peer` and
    /// on the inbound path if the cert chain is parseable. The inbound
    /// path has a gap: peer_identity is `None` under
    /// `with_no_client_auth()`.
    pub cert_pubkey_extraction_failures: AtomicU64,

    // --- DHT connection-level handshake ---
    /// Inbound `peer/1` connection accepted a valid signed `DhtHello`
    /// from the dialer. Bumped once per successful application-layer
    /// handshake — pairs with [`Self::peer_conns_opened`] (which only
    /// counts the *outbound* dial side; the inbound side now uses this
    /// counter to track authenticated-and-admitted peers).
    pub dht_hello_accepted: AtomicU64,

    /// Inbound `peer/1` connection rejected because the dialer's
    /// `DhtHello` failed verification (bad signature, id-pubkey
    /// mismatch, malformed pubkey, stale/future timestamp, or no
    /// hello arrived within the 5s timeout). Bumped once per
    /// rejected connection. See `relay/src/dht/handler.rs` for the
    /// per-failure-mode close-reason mapping.
    pub dht_hello_rejected: AtomicU64,

    // --- sticky-home fan-out ---
    /// `Forward` RPCs the sender relay successfully *opened* (i.e.
    /// invocations that reached the K-fan-out stage). Bumped once per
    /// `forward_to_homes` call regardless of outcome — pair with
    /// `forwards_delivered`/`forwards_stored`/`forward_fallbacks_to_local_queue`
    /// to compute the success-rate.
    pub forwards_sent: AtomicU64,

    /// At least one of the K homes returned `Delivered` (recipient was
    /// online there). Sender acked `DispatchAckP::Delivered` to the
    /// originating client.
    pub forwards_delivered: AtomicU64,

    /// `success >= FORWARD_K_MIN` was reached without any `Delivered`
    /// — i.e. ≥2 of K homes queued the dispatch. Sender acked
    /// `DispatchAckP::Forwarded`.
    pub forwards_stored: AtomicU64,

    /// Fewer than `FORWARD_K_MIN` homes accepted the `Forward`. Sender
    /// fell back to the local queue safety net (§4.2 step 7) and
    /// acked `DispatchAckP::Queued` (or `QueueFull` if the local cap
    /// was hit too).
    pub forward_fallbacks_to_local_queue: AtomicU64,

    /// `enqueue_for_home` writes to `cf_dht_queue` that succeeded —
    /// either the self-is-K-closest path inside `forward_to_homes` or
    /// the inbound `Forward` handler.
    pub dht_queue_writes: AtomicU64,

    /// `enqueue_for_home` rejections because the per-recipient
    /// `MAX_QUEUED_PER_RECIPIENT` cap was hit on `cf_dht_queue`.
    pub dht_queue_full_rejections: AtomicU64,

    // --- sticky-home recipient drain ---

    /// `CRelayPacket::DrainAuth` packets that verified successfully
    /// (signature + freshness window) and were buffered on
    /// `ClientContext.drain_auth`. One bump per accepted packet — a
    /// later refresh (replace-on-set) bumps again. Pair with
    /// `drain_auth_rejected` to compute the verification error rate.
    pub drain_auth_received: AtomicU64,

    /// `CRelayPacket::DrainAuth` packets rejected by
    /// `verify_drain_auth` for any reason (`BadSig`, `StaleTimestamp`,
    /// `FutureTimestamp`). Lumped together so a single counter tells
    /// operators "the drain-auth surface is being abused"; tracing
    /// logs (TRACE level in `drain_auth.rs`) carry the per-reason
    /// breakdown for incident investigation.
    pub drain_auth_rejected: AtomicU64,

    /// `QueueFetch` RPCs the recipient relay sent to home relays
    /// during a `fetch_remote_queues` fan-out. Bumped once per RPC
    /// attempt (per home, per page) regardless of outcome — pair with
    /// `queue_fetch_failures` to compute success rate.
    pub queue_fetches_sent: AtomicU64,

    /// `QueueFetch` RPCs that failed at the wire layer (connect
    /// failed, write/read failed, peer returned the wrong response
    /// variant, total wall-clock budget exhausted). Per-home failures
    /// are non-fatal to the fan-out (`fetch_remote_queues` returns a
    /// per-home best-effort sum).
    pub queue_fetch_failures: AtomicU64,

    /// `fetch_remote_queues` calls that completed and returned at
    /// least one dispatch. Counts the *successful drain* event, not
    /// the per-RPC success — see `queue_fetches_sent` for that.
    pub queue_fetches_succeeded: AtomicU64,

    // --- sticky-home K-set drift migration ---
    /// `cf_dht_queue` entries the periodic scheduler attempted to
    /// migrate to a recipient's new K-closest set. One bump per
    /// candidate `(MessageKey, DispatchP)` returned by
    /// `plan_drift_migrations` and submitted to the migration
    /// driver. Pair with `migrations_succeeded` /
    /// `migrations_failed` to compute the per-sweep churn ratio.
    pub migrations_attempted: AtomicU64,

    /// Migrated entries whose `forward_to_homes` outbound fan-out
    /// reached `FORWARD_K_MIN` "Stored or Delivered" replies and
    /// whose local `cf_dht_queue` row was subsequently deleted. The
    /// scheduler only deletes on success; failures leave the entry
    /// for the next sweep.
    pub migrations_succeeded: AtomicU64,

    /// Migrated entries whose `forward_to_homes` returned
    /// `Err(_)` (insufficient replicas, no homes, etc.) or whose
    /// per-task panicked / cancelled. The local entry is *not*
    /// deleted on failure — the next sweep retries.
    pub migrations_failed: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    // --- lookups ---

    pub fn inc_lookups_started(&self) {
        self.lookups_started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_lookups_succeeded(&self) {
        self.lookups_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_lookups_failed(&self) {
        self.lookups_failed.fetch_add(1, Ordering::Relaxed);
    }

    // --- stores ---

    pub fn inc_stores_received(&self) {
        self.stores_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_stores_accepted(&self) {
        self.stores_accepted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_stores_rejected(&self) {
        self.stores_rejected.fetch_add(1, Ordering::Relaxed);
    }

    // --- RPC kinds ---

    pub fn inc_find_node_rpcs(&self) {
        self.find_node_rpcs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_find_value_rpcs(&self) {
        self.find_value_rpcs.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_pings_sent(&self) {
        self.pings_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_pings_received(&self) {
        self.pings_received.fetch_add(1, Ordering::Relaxed);
    }

    // --- merkle anti-entropy ---

    pub fn inc_merkle_summaries_sent(&self) {
        self.merkle_summaries_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_merkle_summaries_received(&self) {
        self.merkle_summaries_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_merkle_diffs_sent(&self) {
        self.merkle_diffs_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_merkle_diffs_received(&self) {
        self.merkle_diffs_received.fetch_add(1, Ordering::Relaxed);
    }

    // --- routing-table & peer lifecycle ---

    pub fn inc_bucket_evictions(&self) {
        self.bucket_evictions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_peer_conns_opened(&self) {
        self.peer_conns_opened.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_peer_conns_closed(&self) {
        self.peer_conns_closed.fetch_add(1, Ordering::Relaxed);
    }

    // --- DoS hardening ---

    pub fn inc_rate_limit_rejections(&self) {
        self.rate_limit_rejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cert_pubkey_extraction_failures(&self) {
        self.cert_pubkey_extraction_failures.fetch_add(1, Ordering::Relaxed);
    }

    // --- DHT connection-level handshake ---

    pub fn inc_dht_hello_accepted(&self) {
        self.dht_hello_accepted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_dht_hello_rejected(&self) {
        self.dht_hello_rejected.fetch_add(1, Ordering::Relaxed);
    }

    // --- sticky-home fan-out ---

    pub fn inc_forwards_sent(&self) {
        self.forwards_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_forwards_delivered(&self) {
        self.forwards_delivered.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_forwards_stored(&self) {
        self.forwards_stored.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_forward_fallbacks_to_local_queue(&self) {
        self.forward_fallbacks_to_local_queue.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_dht_queue_writes(&self) {
        self.dht_queue_writes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_dht_queue_full_rejections(&self) {
        self.dht_queue_full_rejections.fetch_add(1, Ordering::Relaxed);
    }

    // --- sticky-home recipient drain ---

    pub fn inc_drain_auth_received(&self) {
        self.drain_auth_received.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_drain_auth_rejected(&self) {
        self.drain_auth_rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_queue_fetches_sent(&self) {
        self.queue_fetches_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_queue_fetch_failures(&self) {
        self.queue_fetch_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_queue_fetches_succeeded(&self) {
        self.queue_fetches_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    // --- sticky-home K-set drift migration ---

    pub fn inc_migrations_attempted(&self) {
        self.migrations_attempted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_migrations_succeeded(&self) {
        self.migrations_succeeded.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_migrations_failed(&self) {
        self.migrations_failed.fetch_add(1, Ordering::Relaxed);
    }
}
