use common::quic::CloseReason;
use common::warn;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// For handling incoming connection from a resolveer
impl Handler {
    /// The relay talks to a resolver only as a client, so an inbound
    /// `resolver/N` is unserved. Not reachable via ALPN negotiation — the
    /// role is not in the endpoint's advertised list.
    pub async fn handle_resolver(self, relay: RelayRef) {
        let _ = relay;
        warn!("resolver({}) role is not served here", self.conn.remote_address());
        CloseReason::UnsupportedRole.close(&self.conn);
    }
}
