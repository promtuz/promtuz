//! DHT constants and the operator-tunable [`DhtConfig`].

use serde::Deserialize;

/// Replication factor: the number of homes per user.
pub const K: usize = 3;

pub const ALPHA: usize = 3;

pub const BUCKET_SIZE: usize = 16;

pub const BUCKETS: usize = 256;

/// After a lookup round's first reply, how long it waits for each further one.
pub const LOOKUP_HEDGE_MS: u64 = 150;

/// Bounds each lookup RPC and also the whole walk.
pub const LOOKUP_RPC_TIMEOUT_MS: u64 = 1500;

pub const LOOKUP_MAX_HOPS: u32 = 8;

pub const MAX_LOOKUP_CANDIDATES: usize = 64;

/// The scheduler tick that retries bootstrap and sweeps the rate limiters.
pub const ANTI_ENTROPY_INTERVAL_MS: u64 = 30_000;

pub const BUCKET_REFRESH_MS: u64 = 3_600_000;

/// Budget for a fan-out to the homes. A home that has not answered by then counts as failed.
pub const FORWARD_TIMEOUT_MS: u64 = 1500;

/// Home-side live delivery attempt inside a `Forward`. What remains of the
/// sender's [`FORWARD_TIMEOUT_MS`] covers the round trip and the queue write.
pub const FORWARD_LIVE_DELIVERY_MS: u64 = 1000;

pub const FORWARD_K_MIN: usize = 2;

/// A sole home satisfies a write by itself. `home_count` counts the homes selected before any RPC,
/// never the ones that answered, and zero homes still fails.
pub(crate) fn write_quorum(home_count: usize) -> usize {
    if home_count == 1 { 1 } else { FORWARD_K_MIN }
}

/// Budget for a drain's fetch from the homes. Homes delete nothing until the ack, so a fetch that
/// times out only delays messages to a later drain.
pub const QUEUE_FETCH_TIMEOUT_MS: u64 = 3000;

/// Caps one drift sweep so a full queue cannot stall the scheduler. The next sweep continues.
pub const MAX_MIGRATE_PER_SWEEP: usize = 256;

pub const MAX_CONCURRENT_MIGRATIONS: usize = 8;

pub const RATE_LIMIT_CHEAP_PER_SEC: u32 = 1_000;
pub const RATE_LIMIT_CHEAP_BURST: u32 = 500;

pub const RATE_LIMIT_EXPENSIVE_PER_SEC: u32 = 200;
pub const RATE_LIMIT_EXPENSIVE_BURST: u32 = 100;

pub const RATE_LIMIT_BULK_PER_SEC: u32 = 500;
pub const RATE_LIMIT_BULK_BURST: u32 = 250;

/// Unkeyed ceiling across all peers and classes. A NodeId costs one keygen to mint, so the per-peer
/// quotas alone bound nothing in aggregate.
pub const RATE_LIMIT_GLOBAL_PER_SEC: u32 = 10_000;
pub const RATE_LIMIT_GLOBAL_BURST: u32 = 5_000;

/// Protocol parameters stay constants above, since every relay must agree on them.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct DhtConfig {
    /// When `false` the relay runs without a DHT.
    #[serde(default)]
    pub enabled: bool,

    /// Allows loopback and private peer addresses, for single-host test clusters. In production a
    /// peer-supplied internal address is an SSRF primitive.
    #[serde(default)]
    pub allow_local_peer_addrs: bool,
}
