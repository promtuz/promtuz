use std::sync::Arc;

use crate::quic::endpoint::QuicEndpoint;

pub async fn run_acceptor(ep: Arc<QuicEndpoint>) {
    while let Some(conn) = ep.endpoint.accept().await {
        tokio::spawn(async move {
            if let Ok(_connection) = conn.await {
                todo!("RECOGNIZE ALPN, RESPOND ACCORDINGLY")
            }
        });
    }
}
