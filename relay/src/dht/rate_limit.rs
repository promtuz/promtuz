//! Inbound DHT RPC rate limiters.
//!
//! Without these, a misbehaving peer can hammer the sticky-home or MLS
//! stash RPCs without tripping any per-connection or per-RPC defence.
//! Three keyed `governor` limiters — one per RPC cost class (cheap /
//! expensive / bulk), each keyed on the *requester* NodeId — sit under a
//! single unkeyed global limiter. Tripping any of them closes the
//! inbound connection with `CloseReason::DhtFlood` and bumps a metrics
//! counter.
//!
//! ## Why three classes
//!
//! The cost of an RPC drives the quota — see the `RATE_LIMIT_*`
//! constants in `super::config` for the values and their sizing:
//!
//! - **Cheap** (`FindNode`): no signature verification, no disk I/O; a routing-table read and a
//!   bounded descriptor list back.
//! - **Expensive verify** (`Forward`, `QueueFetch`, `QueueFetchAck`, the presence/live RPCs, the
//!   MLS KeyPackage family): at least one Ed25519 verify plus a synced fjall write or a bounded
//!   prefix scan.
//! - **Bulk** (the MLS Welcome family): the heaviest payloads in the DHT family — `welcome_blob`
//!   reaches `MAX_WELCOME_BYTES`, and a fetch returns up to `MAX_WELCOMES_PER_RECIPIENT` rows.
//!
//! ## Why a global limiter on top of the per-NodeId ones
//!
//! A NodeId is one Ed25519 keygen plus one QUIC handshake, so an
//! attacker multiplies a per-NodeId quota by however many identities it
//! cares to mint: the keyed limiters bound *fairness between honest
//! peers*, not aggregate load. The unkeyed
//! [`PerPeerLimiters::global`] bucket is what actually bounds this
//! relay's inbound RPC work. Per-source-IP admission control belongs a
//! layer lower, in the QUIC acceptor.
//!
//! ## Lock contract
//!
//! `governor::RateLimiter` is internally lock-free under the
//! `DefaultKeyedStateStore` (DashMap-backed). Calls do not block.

use std::num::NonZeroU32;

use common::quic::id::NodeId;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::InMemoryState;
use governor::state::NotKeyed;
use governor::state::keyed::DefaultKeyedStateStore;

use super::config::RATE_LIMIT_BULK_BURST;
use super::config::RATE_LIMIT_BULK_PER_SEC;
use super::config::RATE_LIMIT_CHEAP_BURST;
use super::config::RATE_LIMIT_CHEAP_PER_SEC;
use super::config::RATE_LIMIT_EXPENSIVE_BURST;
use super::config::RATE_LIMIT_EXPENSIVE_PER_SEC;
use super::config::RATE_LIMIT_GLOBAL_BURST;
use super::config::RATE_LIMIT_GLOBAL_PER_SEC;

/// Keyed limiter type alias — one entry per NodeId, with automatic
/// eviction of idle entries (`DefaultKeyedStateStore` handles that
/// internally so we don't carry per-peer state forever after a peer
/// disconnects).
type NodeLimiter = RateLimiter<NodeId, DefaultKeyedStateStore<NodeId>, DefaultClock>;

/// Unkeyed limiter type alias for the aggregate budget.
type GlobalLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Three per-peer limiters, one per RPC cost class, plus the aggregate
/// budget every inbound RPC also draws from.
#[derive(Debug)]
pub(crate) struct PerPeerLimiters {
    pub cheap: NodeLimiter,
    pub expensive: NodeLimiter,
    pub bulk: NodeLimiter,
    pub global: GlobalLimiter,
}

impl PerPeerLimiters {
    pub(crate) fn new() -> Self {
        Self {
            cheap: build_limiter(RATE_LIMIT_CHEAP_PER_SEC, RATE_LIMIT_CHEAP_BURST),
            expensive: build_limiter(RATE_LIMIT_EXPENSIVE_PER_SEC, RATE_LIMIT_EXPENSIVE_BURST),
            bulk: build_limiter(RATE_LIMIT_BULK_PER_SEC, RATE_LIMIT_BULK_BURST),
            global: RateLimiter::direct(quota(
                RATE_LIMIT_GLOBAL_PER_SEC,
                RATE_LIMIT_GLOBAL_BURST,
            )),
        }
    }
}

/// `per_second(rate).allow_burst(burst)`. Constants come from
/// `config.rs`; we use `NonZeroU32::MIN` (= 1) as a defensive fallback
/// in case a future edit zeros one of them, mirroring the resolver
/// acceptor pattern.
fn quota(rate_per_sec: u32, burst: u32) -> Quota {
    let rate = NonZeroU32::new(rate_per_sec).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);
    Quota::per_second(rate).allow_burst(burst)
}

fn build_limiter(rate_per_sec: u32, burst: u32) -> NodeLimiter {
    RateLimiter::keyed(quota(rate_per_sec, burst))
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
            // `FindNode` is a routing-table read and nothing else.
            DhtRequest::FindNode(_) => RpcClass::Cheap,
            // Sticky-home: `Forward` does an outer-sig verify plus a
            // disk write (queue) or stream open (deliver).
            // `QueueFetch` does a user-sig verify plus a per-recipient
            // prefix iterator over `cf_dht_queue`; `QueueFetchAck` a
            // user-sig verify plus a delete per acked id. MLS KeyPackage
            // publish / fetch / refill do Ed25519 verifies plus fjall
            // I/O — same cost shape. A separate per-pair
            // `(target_ipk, requester_relay_id)` quota lives inside
            // `mls/kp.rs` for the anti-pinning policy; this per-peer
            // bucket is the coarser first line.
            DhtRequest::QueueFetchAck(_)
            | DhtRequest::Forward(_)
            | DhtRequest::ActivityForward(_)
            | DhtRequest::PresenceConsent(_)
            | DhtRequest::PresenceState(_)
            | DhtRequest::PresenceLease(_)
            | DhtRequest::LiveForward(_)
            | DhtRequest::PushPseudonymPublish(_)
            | DhtRequest::QueueFetch(_)
            | DhtRequest::KeyPackagePublish(_)
            | DhtRequest::KeyPackageFetch(_)
            | DhtRequest::KeyPackageRefill(_) => RpcClass::Expensive,
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
    /// Draw one token from both the aggregate budget and the
    /// `peer`-keyed limiter for this RPC class. Returns `Err(())` if
    /// either is exhausted.
    pub(crate) fn check(&self, peer: &NodeId, class: RpcClass) -> Result<(), ()> {
        self.global.check().map_err(|_| ())?;
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
        use common::proto::dht_p2p::FindNode;
        use common::proto::dht_p2p::QueueFetchAck;

        let dummy_id = NodeId::from_bytes([0u8; 32]);
        let find_node =
            DhtRequest::FindNode(FindNode { target: [0u8; 32].into(), requester: dummy_id });
        assert_eq!(RpcClass::for_request(&find_node), RpcClass::Cheap);

        // The ack verifies a user signature and deletes a row per id.
        let ack = DhtRequest::QueueFetchAck(QueueFetchAck {
            user_ipk:           [0u8; 32].into(),
            requester_relay_id: dummy_id,
            delivered_ids:      vec![[0u8; 16]],
            timestamp:          0,
            user_sig:           [0u8; 64].into(),
        });
        assert_eq!(RpcClass::for_request(&ack), RpcClass::Expensive);
    }

    #[test]
    fn global_budget_denies_a_peer_that_is_under_its_own_quota() {
        use super::super::config::RATE_LIMIT_GLOBAL_BURST;

        let limiters = PerPeerLimiters::new();
        // Spread the drain across distinct peers, each staying well
        // under `RATE_LIMIT_CHEAP_BURST`, so only the global bucket can
        // be the one that trips.
        for i in 0..(RATE_LIMIT_GLOBAL_BURST as usize) {
            let peer = NodeId::from_bytes([(i % 251) as u8; 32]);
            let _ = limiters.check(&peer, RpcClass::Cheap);
        }
        let fresh = id_from_seed(0xC3);
        let denied = (0..200).filter(|_| limiters.check(&fresh, RpcClass::Cheap).is_err()).count();
        assert!(denied > 0, "global budget must deny once its burst is drained");
    }

    #[test]
    fn limiter_grants_burst_then_denies() {
        // Time-based quotas under `governor` are forgiving in test
        // environments (real-time wall clock), so we don't measure the
        // steady-state rate, only the burst behaviour.
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
