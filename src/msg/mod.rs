use crate::quic::id::NodeId;

pub mod pack;
pub mod client;
pub mod reason;
pub mod relay;
pub mod resolver;

pub type UserId = String;
pub type RelayId = NodeId;
pub type ResolverId = NodeId;
