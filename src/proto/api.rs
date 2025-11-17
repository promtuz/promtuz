use common::msg::RelayId;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Serialize, Deserialize)]
pub struct RelayDescriptor {
    pub id: RelayId,
    pub addr: String,
}
