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

    // --- DoS hardening (phase 1h) ---
    /// Per-peer rate limit was tripped on an inbound RPC. Bumped once
    /// per rejected stream — multiple denied calls within one
    /// connection still bump exactly once before the close.
    pub rate_limit_rejections: AtomicU64,

    /// Inbound `peer/1` connection rejected because the post-handshake
    /// TLS-pubkey extraction failed (cert chain absent, malformed
    /// SPKI, self-sig invalid, or `BLAKE3(spki) != claimed_node_id`).
    /// Bumped on the dial-side path in `lookup::connect_to_peer` and
    /// on the inbound path if the cert chain is parseable. See item 1
    /// in the phase 1h dispatch report for the inbound-path gap
    /// (peer_identity is `None` under `with_no_client_auth()`).
    pub cert_pubkey_extraction_failures: AtomicU64,
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

    // --- DoS hardening (phase 1h) ---

    pub fn inc_rate_limit_rejections(&self) {
        self.rate_limit_rejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cert_pubkey_extraction_failures(&self) {
        self.cert_pubkey_extraction_failures.fetch_add(1, Ordering::Relaxed);
    }
}
