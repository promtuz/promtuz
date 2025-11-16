use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub trait HandleClient {
    async fn handle_client(self, resolver: ResolverRef);
}

impl HandleClient for Handler {
    async fn handle_client(self, resolver: ResolverRef) {
      unimplemented!()
    }
}
