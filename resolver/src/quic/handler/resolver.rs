use common::quic::CloseReason;
use common::warn;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub trait HandleResolver {
    async fn handle_resolver(self, resolver: ResolverRef);
}

impl HandleResolver for Handler {
    async fn handle_resolver(self, _resolver: ResolverRef) {
        warn!(
            "resolver-role connection from {}: not implemented",
            self.conn.remote_address()
        );
        CloseReason::UnsupportedRole.close(&self.conn);
    }
}
