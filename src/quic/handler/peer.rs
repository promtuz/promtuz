use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// For handling incoming connection from other relays in network
impl Handler {
    pub async fn handle_peer(self, relay: RelayRef) {
        todo!()
    }
}