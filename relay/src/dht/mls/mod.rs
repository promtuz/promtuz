//! MLS stash relay: the KeyPackage and Welcome stashes at their homes, and the originating side
//! that fans a client's request out to them.

pub(crate) mod inventory;
pub(crate) mod kp;
pub(crate) mod kp_originate;
pub(crate) mod welcome;
pub(crate) mod welcome_originate;

use std::num::NonZeroU32;

use common::proto::mls_wire::MAX_KP_SKEW_MS;
use common::quic::id::NodeId;
use governor::Quota;

use crate::dht::Dht;

/// Guarantees implemented by this relay's actual storage owner. Used on both
/// client and peer connections; delegation checks the target independently.
pub(crate) fn service_support() -> common::contracts::Support {
    use common::contracts::{Support,services};
    Support::new([(services::KEY_PACKAGE_CUSTODY,vec![services::KEY_PACKAGE_CUSTODY_VERSION]), (services::KEY_PACKAGE_INVENTORY,vec![services::KEY_PACKAGE_INVENTORY_VERSION])]).expect("fixed supported service versions")
}

fn hourly_quota(per_hour: u32) -> Quota {
    let period = std::time::Duration::from_secs(3600 / per_hour.max(1) as u64);
    let burst = NonZeroU32::new(per_hour).unwrap_or(NonZeroU32::MIN);
    Quota::with_period(period).expect("non-zero period per token").allow_burst(burst)
}

#[derive(Debug)]
enum Reject {
    Binding,
    Skew,
    RateLimited,
    NotOwner,
}

/// The stash RPCs' admission, before any signature or disk work. A named requester must be the
/// authenticated peer, against cross-relay replay, and `admit` is spent only on a fresh request.
fn stash_gate(
    dht: &Dht, peer: NodeId, requester: Option<NodeId>, timestamp: u64, now_ms: u64,
    stash: [u8; 32], admit: impl FnOnce() -> bool,
) -> Result<(), Reject> {
    if requester.is_some_and(|requester| requester != peer) {
        return Err(Reject::Binding);
    }
    if now_ms.abs_diff(timestamp) > MAX_KP_SKEW_MS {
        return Err(Reject::Skew);
    }
    if !admit() {
        return Err(Reject::RateLimited);
    }
    if !crate::dht::routing::homes(dht, &NodeId::from_bytes(stash)).1 {
        return Err(Reject::NotOwner);
    }
    Ok(())
}
