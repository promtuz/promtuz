use serde::{Deserialize, Serialize};

use crate::msg::NodeId;

/// Initial registration message sent by a relay node to a resolver.
///
/// This announces the node's identity, network address, and basic
/// capabilities so the resolver can track it in the live node set.
#[derive(Serialize, Deserialize)]
pub struct NodeHello {
    /// Stable cryptographic ID derived from the node's public key.
    pub node_id: NodeId,
    pub address: (String, u16),
    pub version: i16,
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
    pub reason: Option<String>,

    /// Resolver's current unix time (used for clock-drift checking).
    pub resolver_time: u64,

    /// Heartbeat interval the node should follow when sending `NodeHeartbeat`.
    pub interval_heartbeat_ms: u32,
}

/// Periodic heartbeat sent by a node to indicate that it is still alive
/// and to provide useful runtime metrics to the resolver.
#[derive(Serialize, Deserialize, Debug)]
pub struct NodeHeartbeat {
    /// The node's stable cryptographic ID.
    pub node_id: String,

    /// Current load percentage (0–100). Interpreted by your resolver logic.
    pub load: u8,

    /// Node uptime in seconds since its last restart.
    pub uptime_seconds: u64,
}
