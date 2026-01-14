use anyhow::Result;

use common::proto::client_res::RelayDescriptor;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
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
