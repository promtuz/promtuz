use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub trait HandleResolver {
    async fn handle_resolver(self, resolver: ResolverRef);
}

impl HandleResolver for Handler {
    async fn handle_resolver(self, resolver: ResolverRef) {
        unimplemented!()
    }
}
