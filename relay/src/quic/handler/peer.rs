use common::debug;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// For handling incoming connection from other relays in network
impl Handler {
    pub async fn handle_peer(self, _relay: RelayRef) {
        let conn = self.conn.clone();
        let remote_addr = conn.remote_address();
        debug!("connection from peer({remote_addr})");

        // UNIMPLEMENTED
    }
}