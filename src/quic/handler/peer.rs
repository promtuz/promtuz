use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

pub trait HandlePeer {
    async fn handle_peer(self, relay: RelayRef);
}


impl HandlePeer for Handler {
    async fn handle_peer(self, relay: RelayRef) {
        todo!()
    }
}