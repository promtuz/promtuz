use crate::quic::id::NodeId;

pub mod resolver;
pub mod cbor;
pub mod reason;

pub type UserId = String;
pub type RelayId = NodeId;
pub type ResolverId = NodeId;