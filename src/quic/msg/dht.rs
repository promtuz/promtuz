use std::net::SocketAddr;

use common::quic::id::NodeId;
use serde::{Deserialize, Serialize};

use crate::dht::{NodeContact, UserRecord};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DhtRequest {
    Ping { from: NodeId, addr: SocketAddr },
    StoreUser { record: UserRecord },
    FindUser { ipk: [u8; 32] },
    FindNode { target: NodeId },
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DhtResponse {
    Pong { from: NodeId },
    StoreOk,
    UserResult { records: Vec<UserRecord> },
    NodeResult { nodes: Vec<NodeContact> },
    Error { reason: String },
}
