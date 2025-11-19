use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

pub trait HandleClient {
    async fn handle_client(self, relay: RelayRef);
}


impl HandleClient for Handler {
    async fn handle_client(self, relay: RelayRef) {
        todo!()
    }
}