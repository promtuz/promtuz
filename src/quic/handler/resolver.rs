use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

pub trait HandleResolver {
    async fn handle_resolver(self, relay: RelayRef);
}


impl HandleResolver for Handler {
    async fn handle_resolver(self, relay: RelayRef) {
        todo!()
    }
}