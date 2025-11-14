use std::sync::Arc;

use quinn::Endpoint;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

/// Accepts all incoming connections for given endpoint and handles them accordingly
pub struct Acceptor {
    /// Clone of endpoint reference from [Resolver]
    endpoint: Arc<Endpoint>,
}

impl Acceptor {
    pub fn new(endpoint: Arc<Endpoint>) -> Self {
        Self { endpoint }
    }

    pub async fn run(&self, resolver: ResolverRef) {
        while let Some(conn) = self.endpoint.accept().await {
            let resolver = resolver.clone();
            tokio::spawn(async move {
                if let Ok(connection) = conn.await {
                    Handler::handle(connection, resolver).await;
                }
            });
        }
    }
}
