use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// For handling incoming connection from a resolveer
impl Handler {
    pub async fn handle_resolver(self, relay: RelayRef) {
        let _ = relay;
        todo!()
    }
}
