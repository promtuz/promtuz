use serde::Deserialize;
use serde::Serialize;

use crate::proto::api::RelayDescriptor;

///
#[derive(Debug, Serialize, Deserialize)]
pub enum ClientRequest {
    GetRelays(),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientResponse {
    /// Resolver's response to [ClientRequest::GetRelays]
    GetRelays { relays: Vec<RelayDescriptor> },
}
