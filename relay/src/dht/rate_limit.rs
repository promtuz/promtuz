//! Inbound DHT RPC limiters: one per cost class keyed by peer, under one unkeyed global limiter.

use std::hash::Hash;
use std::num::NonZeroU32;

use common::quic::id::NodeId;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::Clock;
use governor::clock::DefaultClock;
use governor::middleware::NoOpMiddleware;
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

/// Tests run the keyed limiters on governor's fake clock, so refill and sweeping take no waiting.
#[cfg(not(test))]
pub(crate) type LimiterClock = DefaultClock;
#[cfg(test)]
pub(crate) type LimiterClock = governor::clock::FakeRelativeClock;

pub(crate) type KeyedLimiter<K> = RateLimiter<
    K,
    DefaultKeyedStateStore<K>,
    LimiterClock,
    NoOpMiddleware<<LimiterClock as Clock>::Instant>,
>;

pub(crate) fn keyed<K: Clone + Hash + Eq>(quota: Quota, clock: &LimiterClock) -> KeyedLimiter<K> {
    RateLimiter::new(quota, DefaultKeyedStateStore::default(), clock.clone())
}

type GlobalLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

#[derive(Debug)]
pub(crate) struct PerPeerLimiters {
    pub cheap: KeyedLimiter<NodeId>,
    pub expensive: KeyedLimiter<NodeId>,
    pub bulk: KeyedLimiter<NodeId>,
    pub global: GlobalLimiter,
}

impl PerPeerLimiters {
    pub(crate) fn new(clock: &LimiterClock) -> Self {
        Self {
            cheap: keyed(quota(RATE_LIMIT_CHEAP_PER_SEC, RATE_LIMIT_CHEAP_BURST), clock),
            expensive: keyed(quota(RATE_LIMIT_EXPENSIVE_PER_SEC, RATE_LIMIT_EXPENSIVE_BURST), clock),
            bulk: keyed(quota(RATE_LIMIT_BULK_PER_SEC, RATE_LIMIT_BULK_BURST), clock),
            global: RateLimiter::direct(quota(
                RATE_LIMIT_GLOBAL_PER_SEC,
                RATE_LIMIT_GLOBAL_BURST,
            )),
        }
    }
}

fn quota(rate_per_sec: u32, burst: u32) -> Quota {
    let rate = NonZeroU32::new(rate_per_sec).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);
    Quota::per_second(rate).allow_burst(burst)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcClass {
    Cheap,
    Expensive,
    Bulk,
}

impl RpcClass {
    pub(crate) fn for_request(req: &common::proto::dht_p2p::DhtRequest) -> Self {
        use common::proto::dht_p2p::DhtRequest;
        match req {
            // No signature check and no disk I/O.
            DhtRequest::FindNode(_) | DhtRequest::ServiceCapabilities => RpcClass::Cheap,
            // At least one Ed25519 verify plus a synced write or a bounded prefix scan.
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
            | DhtRequest::KeyPackageRefill(_)
            | DhtRequest::KeyPackageInventory { .. } => RpcClass::Expensive,
            // The largest payloads: a `welcome_blob` reaches `MAX_WELCOME_BYTES`.
            DhtRequest::WelcomePublish(_)
            | DhtRequest::WelcomeFetch(_)
            | DhtRequest::WelcomeAck(_) => RpcClass::Bulk,
        }
    }
}

impl PerPeerLimiters {
    pub(crate) fn sweep(&self) {
        for l in [&self.cheap, &self.expensive, &self.bulk] {
            l.retain_recent();
            l.shrink_to_fit();
        }
    }

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
