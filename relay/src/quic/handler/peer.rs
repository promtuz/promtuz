use common::info;
use common::quic::CloseReason;
use common::warn;

use crate::dht::handler::handle_peer_connection;
use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// Inbound `peer/5` handler — the relay-to-relay surface.
///
/// When the DHT is enabled, hand the connection off to the DHT's per-
/// connection driver (`dht::handler::handle_peer_connection`). When it
/// is disabled, close the connection with `UnsupportedRole` so the
/// dialer can stop retrying.
impl Handler {
    pub async fn handle_peer(self, relay: RelayRef) {
        let conn = self.conn.clone();
        let remote_addr = conn.remote_address();

        match relay.dht.as_ref() {
            Some(dht) => {
                info!("DHT peer connection accepted from {remote_addr}");
                handle_peer_connection(dht.clone(), conn).await;
            },
            None => {
                warn!(
                    "received peer/5 connection from {remote_addr} but DHT is disabled; closing"
                );
                CloseReason::UnsupportedRole.close(&conn);
            },
        }
    }
}
