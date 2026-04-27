use common::quic::CloseReason;
use common::warn;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub trait HandleResolver {
    async fn handle_resolver(self, resolver: ResolverRef);
}

impl HandleResolver for Handler {
    /// Resolver-to-resolver gossip is not yet implemented. A peer that
    /// negotiates the `resolver/1` ALPN today gets a polite close with
    /// [`CloseReason::UnsupportedRole`] rather than panicking the spawned
    /// task — `tokio::spawn` would otherwise swallow the panic but leak
    /// the connection (no close, no cleanup) and flood stderr with
    /// backtraces on every probe.
    async fn handle_resolver(self, _resolver: ResolverRef) {
        warn!(
            "resolver-role connection from {}: not implemented",
            self.conn.remote_address()
        );
        CloseReason::UnsupportedRole.close(&self.conn);
    }
}
