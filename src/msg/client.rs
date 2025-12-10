use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::msg::RelayId;


#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct RelayDescriptor {
    pub id: RelayId,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr: SocketAddr,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientRequest {
    GetRelays(),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientResponse {
    /// Resolver's response to [ClientRequest::GetRelays]
    GetRelays { relays: Vec<RelayDescriptor> },
}
