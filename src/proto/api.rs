use std::net::SocketAddr;

use common::msg::RelayId;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize)]
pub struct RelayDescriptor {
    id: RelayId,
    addr: SocketAddr,
}
