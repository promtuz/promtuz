use std::sync::Arc;

use quinn::Endpoint;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// Accepts all incoming connections for given endpoint and handles them accordingly
pub struct Acceptor {
    /// Clone of endpoint reference from [Relay]
    endpoint: Arc<Endpoint>,
}

impl Acceptor {
    pub fn new(endpoint: Arc<Endpoint>) -> Self {
        Self { endpoint }
    }

    pub async fn run(&self, relay: RelayRef) {
        while let Some(conn) = self.endpoint.accept().await {
            let relay = relay.clone();
            tokio::spawn(async move {
                if let Ok(connection) = conn.await {
                    Handler::handle(connection, relay).await;
                }
            });
        }
    }
}
