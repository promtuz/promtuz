use anyhow::Result;

use crate::proto::api::RelayDescriptor;
use crate::proto::client::ClientRequest;
use crate::proto::client::ClientResponse;
use crate::resolver::Resolver;

pub trait HandleRPC {
    async fn handle_rpc(&self, req: ClientRequest) -> Result<ClientResponse>;
}

impl HandleRPC for Resolver {
    async fn handle_rpc(&self, req: ClientRequest) -> Result<ClientResponse> {
        match req {
            ClientRequest::GetRelays() => {
                let relays: Vec<RelayDescriptor> =
                    self.relays.values().map(|r| r.to_descriptor()).collect();
                Ok(ClientResponse::GetRelays { relays })
            },
        }
    }
}
