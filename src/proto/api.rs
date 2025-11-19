use std::net::SocketAddr;

use common::msg::RelayId;
use serde::Deserialize;
use serde::Serialize;

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