//! cli - client
//! rel - relay
//! res - resolver

use crate::quic::id::NodeId;

pub mod client_peer;
pub mod client_rel;
pub mod client_res;
pub mod pack;
pub mod peer;
pub mod relay_peer;
pub mod relay_res;

pub type RelayId = NodeId;
pub type ResolverId = NodeId;
