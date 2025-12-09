use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::msg::RelayId;


#[derive(Debug, Serialize, Deserialize)]
pub struct RelayDescriptor {
    pub id: RelayId,
    #[serde(serialize_with = "ser_addr")]
    pub addr: SocketAddr,
}

fn ser_addr<S>(addr: &SocketAddr, s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_str(&addr.to_string())
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
