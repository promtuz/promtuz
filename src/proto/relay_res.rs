//! Relay to Resolver Proto

use std::fmt::Debug;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{
    proto::{
        RelayId,
        pack::{Packable, Packer},
    },
    sysutils::SystemLoad,
};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum LifetimeP {
    /// Initial registration message sent by a relay node to a resolver.
    ///
    /// This announces the node's identity, network address, and basic
    /// capabilities so the resolver can track it in the live node set.
    RelayHello {
        /// Stable cryptographic ID derived from the node's public key.
        relay_id: RelayId,
        timestamp: u128,
        // TODO: I'd rather use bitset
        // pub capabilities: Vec<String>,
    },

    /// Resolver's acknowledgement of a node registration (`NodeHello`).
    ///
    /// Confirms acceptance, conveys heartbeat timing, or explains rejection.
    HelloAck {
        /// Whether the resolver accepts this node into the active set.
        accepted: bool,

        /// Human-readable reason if `accepted == false`.
        /// I wonder what human will read this reason,
        /// TODO: use `enum RelayRejectReason` or something
        reason: Option<String>,

        /// Resolver's current unix time (used for clock-drift checking).
        resolver_time: u128,

        /// Heartbeat interval the node should follow when sending `NodeHeartbeat`.
        interval_heartbeat_ms: u32,
    },

    /// Periodic heartbeat sent by a node to indicate that it is still alive
    /// and to provide useful runtime metrics to the resolver.
    RelayHeartbeat {
        /// The node's stable cryptographic ID.
        relay_id: RelayId,

        /// Packed load value:
        ///
        /// upper 7 bits = CPU usage (0–100), lower 7 bits = memory usage (0–100).
        load: SystemLoad,

        /// Node uptime in seconds since its last restart.
        uptime_seconds: u64,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ResolverPacket {
    Lifetime(LifetimeP),
}

impl Packable for ResolverPacket {}

impl ResolverPacket {
    pub async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> anyhow::Result<()> {
        let packet = self.pack()?;

        tx.write_all(&packet).await?;
        Ok(tx.flush().await?)
    }
}
