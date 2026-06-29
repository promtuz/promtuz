//! Per-peer inbound DHT RPC rate limiters.
//!
//! Without these, a misbehaving peer can hammer Store/Tombstone or
//! FetchRecord RPCs without tripping any per-connection or per-RPC
//! defence. We use three keyed `governor` limiters — one per RPC
//! class (cheap / expensive / bulk) — each keyed on the *requester*
//! NodeId. Tripping any of them closes the inbound connection with
//! `CloseReason::DhtFlood` and bumps a metrics counter.
//!
//! ## Why three classes
//!
//! The cost of an RPC drives the quota:
//!
//! - **Cheap** (`Ping`, `FindNode`, `FindValue`, `MerkleSummary`,
//!   `MerkleDiff`): zero crypto verification, only routing-table
//!   reads. 100/s sustained, burst 50.
//! - **Expensive verify** (`Store`, `Tombstone`): each does an
//!   Ed25519 verify (~100 µs) + a sync fjall write. 20/s sustained,
//!   burst 10.
//! - **Bulk** (`FetchRecord`): bounded by `MAX_FETCH_RECORD_BATCH`
//!   per request, so each RPC is itself an O(64) read amplification.
//!   50/s sustained, burst 25.
//!
//! ## Why per-NodeId, not per-IP
//!
//! The acceptor at `relay/src/quic/acceptor.rs` does not — yet — do
//! per-IP rate limiting (only per-connection concurrency capping).
//! Per-IP at the QUIC accept layer is the resolver's pattern
//! (`resolver/src/quic/acceptor.rs`); for relay-to-relay traffic the
//! `NodeId` is a stronger key because:
//! - A misbehaving peer cannot evade the limit by reconnecting from a
//!   new socket — its NodeId is cryptographically fixed
//!   (`BLAKE3(spki)`).
//! - A NAT'd legitimate peer and a misbehaving peer behind the same
//!   NAT do not share a quota.
//!
//! ## Lock contract
//!
//! `governor::RateLimiter` is internally lock-free under the
//! `DefaultKeyedStateStore` (DashMap-backed). Calls do not block.
//!

use std::num::NonZeroU32;

use common::quic::id::NodeId;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;

use super::config::RATE_LIMIT_BULK_BURST;
use super::config::RATE_LIMIT_BULK_PER_SEC;
use super::config::RATE_LIMIT_CHEAP_BURST;
use super::config::RATE_LIMIT_CHEAP_PER_SEC;
use super::config::RATE_LIMIT_EXPENSIVE_BURST;
use super::config::RATE_LIMIT_EXPENSIVE_PER_SEC;

/// Keyed limiter type alias — one entry per NodeId, with automatic
/// eviction of idle entries (`DefaultKeyedStateStore` handles that
/// internally so we don't carry per-peer state forever after a peer
/// disconnects).
type NodeLimiter =
    RateLimiter<NodeId, DefaultKeyedStateStore<NodeId>, DefaultClock>;

/// Three bundled limiters, one per RPC cost class. Cloning a
/// [`PerPeerLimiters`] is just an `Arc` clone of each inner
/// `RateLimiter` (governor's `RateLimiter` is internally `Arc`-able).
#[derive(Debug)]
pub(crate) struct PerPeerLimiters {
    pub cheap: NodeLimiter,
    pub expensive: NodeLimiter,
    pub bulk: NodeLimiter,
}

impl PerPeerLimiters {
    pub(crate) fn new() -> Self {
        Self {
            cheap: build_limiter(RATE_LIMIT_CHEAP_PER_SEC, RATE_LIMIT_CHEAP_BURST),
            expensive: build_limiter(
                RATE_LIMIT_EXPENSIVE_PER_SEC,
                RATE_LIMIT_EXPENSIVE_BURST,
            ),
            bulk: build_limiter(RATE_LIMIT_BULK_PER_SEC, RATE_LIMIT_BULK_BURST),
        }
    }
}

/// Build a single keyed `RateLimiter` with `per_second(rate)` quota
/// and `allow_burst(burst)`. Constants come from `config.rs`; we use
/// `NonZeroU32::MIN` (= 1) as a defensive fallback in case a future
/// edit zeros one of them, mirroring the resolver acceptor pattern.
fn build_limiter(rate_per_sec: u32, burst: u32) -> NodeLimiter {
    let rate = NonZeroU32::new(rate_per_sec).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);
    let quota = Quota::per_second(rate).allow_burst(burst);
    RateLimiter::keyed(quota)
}

/// RPC cost class — one per `DhtRequest` variant. The dispatcher in
/// `handler.rs::handle_dht_request` matches on the request and looks
/// up the corresponding limiter via this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcClass {
    Cheap,
    Expensive,
    Bulk,
}

impl RpcClass {
    /// Map a [`common::proto::dht_p2p::DhtRequest`] variant to its
    /// rate-limit cost class. Centralised so a future RPC variant
    /// can't be classified inconsistently across call-sites.
    pub(crate) fn for_request(req: &common::proto::dht_p2p::DhtRequest) -> Self {
        use common::proto::dht_p2p::DhtRequest;
        match req {
            DhtRequest::Ping(_)
            | DhtRequest::FindNode(_)
            | DhtRequest::FindValue(_)
            | DhtRequest::MerkleSummary(_)
            | DhtRequest::MerkleDiff(_)
            // Sticky-home: ack is a small bookkeeping write (delete by
            // id), no signature-heavy work beyond the bounded id-list
            // verify. Slot it in the cheap bucket.
            | DhtRequest::QueueFetchAck(_) => RpcClass::Cheap,
            DhtRequest::Store(_)
            | DhtRequest::Tombstone(_)
            // Sticky-home: `Forward` does an outer-sig verify plus a
            // disk write (queue) or stream open (deliver).
            // `QueueFetch` does a user-sig verify plus a per-recipient
            // prefix iterator over `cf_dht_queue`. Both belong in the
            // expensive bucket.
            | DhtRequest::Forward(_)
            | DhtRequest::QueueFetch(_)
            // MLS KeyPackage publish / fetch / refill all do Ed25519
            // verifies plus fjall I/O — same cost shape as Store /
            // Forward; expensive class. A separate per-pair
            // `(target_ipk, requester_relay_id)` quota lives inside
            // `mls_kp.rs` for the anti-pinning policy; this per-peer
            // bucket is the coarser first line.
            | DhtRequest::KeyPackagePublish(_)
            | DhtRequest::KeyPackageFetch(_)
            | DhtRequest::KeyPackageRefill(_) => RpcClass::Expensive,
            DhtRequest::FetchRecord(_) => RpcClass::Bulk,
            // MLS welcome publish carries up to a few KB of
            // `welcome_blob` plus envelope metadata; fetch returns up
            // to `MAX_WELCOMES_PER_RECIPIENT = 32` rows in a single
            // RPC; ack is a small id-list. All three are bulk-class
            // because `welcome_blob` can hit
            // `MAX_WELCOME_BYTES = 256 KiB` in the worst case (large
            // groups), making them the heaviest single-RPC payload in
            // the DHT family.
            DhtRequest::WelcomePublish(_)
            | DhtRequest::WelcomeFetch(_)
            | DhtRequest::WelcomeAck(_) => RpcClass::Bulk,
        }
    }
}

impl PerPeerLimiters {
    /// Check the `peer`-keyed limiter for this RPC class. Returns
    /// `Ok(())` if a token was consumed, `Err(())` if the peer is
    /// over quota.
    pub(crate) fn check(&self, peer: &NodeId, class: RpcClass) -> Result<(), ()> {
        let limiter = match class {
            RpcClass::Cheap => &self.cheap,
            RpcClass::Expensive => &self.expensive,
            RpcClass::Bulk => &self.bulk,
        };
        limiter.check_key(peer).map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_from_seed(seed: u8) -> NodeId {
        let mut s = [0u8; 32];
        s[0] = seed;
        NodeId::new(s)
    }

    #[test]
    fn per_peer_limiters_classify_rpcs_correctly() {
        use common::proto::dht_p2p::DhtRequest;
        use common::proto::dht_p2p::FetchRecord;
        use common::proto::dht_p2p::FindNode;
        use common::proto::dht_p2p::FindValue;
        use common::proto::dht_p2p::MerkleDiff;
        use common::proto::dht_p2p::MerkleSummary;
        use common::proto::dht_p2p::Ping;
        use common::proto::dht_p2p::PresenceRecord;
        use common::proto::dht_p2p::Store;
        use common::proto::dht_p2p::Tombstone;
        use common::proto::dht_p2p::TombstoneRecord;

        // Stub records — we don't care about validity, just classification.
        let dummy_id = NodeId::from_bytes([0u8; 32]);
        let dummy_record = PresenceRecord {
            user_ipk:     [0u8; 32].into(),
            relay_id:     dummy_id,
            relay_pubkey: [0u8; 32].into(),
            not_before:   0,
            not_after:    1,
            generation:   0,
            capabilities: 0,
            user_sig:     [0u8; 64].into(),
            relay_sig:    [0u8; 64].into(),
        };
        let dummy_tomb = TombstoneRecord {
            user_ipk:     [0u8; 32].into(),
            relay_id:     dummy_id,
            relay_pubkey: [0u8; 32].into(),
            generation:   0,
            deleted_at:   0,
            relay_sig:    [0u8; 64].into(),
        };

        let cases = [
            (
                DhtRequest::Ping(Ping {
                    nonce:     [0u8; 16].into(),
                    timestamp: 0,
                }),
                RpcClass::Cheap,
            ),
            (
                DhtRequest::FindNode(FindNode {
                    target:    [0u8; 32].into(),
                    requester: dummy_id,
                }),
                RpcClass::Cheap,
            ),
            (
                DhtRequest::FindValue(FindValue {
                    user_ipk:  [0u8; 32].into(),
                    requester: dummy_id,
                }),
                RpcClass::Cheap,
            ),
            (
                DhtRequest::MerkleSummary(MerkleSummary { slices: [0u8; 32].into() }),
                RpcClass::Cheap,
            ),
            (
                DhtRequest::MerkleDiff(MerkleDiff { slice_id: 0, path: vec![] }),
                RpcClass::Cheap,
            ),
            (
                DhtRequest::Store(Store { record: dummy_record }),
                RpcClass::Expensive,
            ),
            (
                DhtRequest::Tombstone(Tombstone { record: dummy_tomb }),
                RpcClass::Expensive,
            ),
            (
                DhtRequest::FetchRecord(FetchRecord { user_ipks: vec![] }),
                RpcClass::Bulk,
            ),
        ];
        for (req, expected) in cases {
            assert_eq!(RpcClass::for_request(&req), expected, "req={req:?}");
        }
    }

    #[test]
    fn limiter_grants_burst_then_denies() {
        // The `expensive` limiter has a 10-burst — verify we can fire
        // ~10 in immediate succession and then get denied. Time-based
        // quotas under `governor` are forgiving in test environments
        // (real-time wall clock), so we don't measure the steady-state
        // rate, only the burst behaviour.
        let limiters = PerPeerLimiters::new();
        let peer = id_from_seed(7);

        // Drain the burst.
        let mut allowed = 0;
        for _ in 0..(RATE_LIMIT_EXPENSIVE_BURST as usize) {
            if limiters.check(&peer, RpcClass::Expensive).is_ok() {
                allowed += 1;
            }
        }
        // We should have been allowed ~the burst size. The
        // `governor` library may smoothly refill mid-loop on a fast
        // CPU, so allow up to burst+1.
        assert!(
            allowed >= (RATE_LIMIT_EXPENSIVE_BURST as usize).saturating_sub(1),
            "expected ~{} allowed in burst, got {}",
            RATE_LIMIT_EXPENSIVE_BURST,
            allowed
        );

        // The very next call (still well inside the same wall-clock
        // millisecond) should be denied because the burst is now
        // exhausted and the steady-state rate hasn't refilled.
        // Fire many in a row to be confident — at least one must
        // trip on a saturated bucket.
        let mut denied = 0;
        for _ in 0..50 {
            if limiters.check(&peer, RpcClass::Expensive).is_err() {
                denied += 1;
            }
        }
        assert!(denied > 0, "expected at least one deny after burst exhausted");
    }

    #[test]
    fn limiter_isolates_per_peer() {
        // Different peers do not share quota.
        let limiters = PerPeerLimiters::new();
        let peer_a = id_from_seed(1);
        let peer_b = id_from_seed(2);

        // Drain peer A's expensive bucket.
        for _ in 0..((RATE_LIMIT_EXPENSIVE_BURST as usize) + 5) {
            let _ = limiters.check(&peer_a, RpcClass::Expensive);
        }

        // Peer B should still get allowed at least once.
        assert!(limiters.check(&peer_b, RpcClass::Expensive).is_ok());
    }
}
