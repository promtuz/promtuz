use std::fmt::Debug;

use serde::{Deserialize, Serialize};

use crate::{msg::RelayId, quic::id::NodeId, sysutils::SystemLoad};

/// Initial registration message sent by a relay node to a resolver.
///
/// This announces the node's identity, network address, and basic
/// capabilities so the resolver can track it in the live node set.
#[derive(Serialize, Deserialize, Debug)]
pub struct RelayHello {
    /// Stable cryptographic ID derived from the node's public key.
    pub relay_id: RelayId,

    pub timestamp: u128,

    // TODO: I'd rather use bitset
    // pub capabilities: Vec<String>,
}

/// Resolver's acknowledgement of a node registration (`NodeHello`).
///
/// Confirms acceptance, conveys heartbeat timing, or explains rejection.
#[derive(Serialize, Deserialize, Debug)]
pub struct HelloAck {
    /// Whether the resolver accepts this node into the active set.
    pub accepted: bool,

    /// Human-readable reason if `accepted == false`.
    /// I wonder what human will read this reason,
    /// TODO: use `enum RelayRejectReason` or something
    pub reason: Option<String>,

    /// Resolver's current unix time (used for clock-drift checking).
    pub resolver_time: u128,

    /// Heartbeat interval the node should follow when sending `NodeHeartbeat`.
    pub interval_heartbeat_ms: u32,
}

/// Periodic heartbeat sent by a node to indicate that it is still alive
/// and to provide useful runtime metrics to the resolver.
#[derive(Serialize, Deserialize, Debug)]
pub struct RelayHeartbeat {
    /// The node's stable cryptographic ID.
    pub relay_id: RelayId,

    /// Packed load value:
    ///
    /// upper 7 bits = CPU usage (0–100), lower 7 bits = memory usage (0–100).
    pub load: SystemLoad,

    /// Node uptime in seconds since its last restart.
    pub uptime_seconds: u64,
}
