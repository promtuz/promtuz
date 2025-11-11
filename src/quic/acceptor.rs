use std::sync::Arc;

use common::msg::protorole::ProtoRole;
use quinn::Connection;

use crate::quic::endpoint::QuicEndpoint;

pub async fn run_acceptor(ep: Arc<QuicEndpoint>) {
    while let Some(conn) = ep.endpoint.accept().await {
        tokio::spawn(async move {
            if let Ok(connection) = conn.await {
                handle_connection(connection).await;
            }
        });
    }
}

pub async fn handle_connection(conn: Connection) -> Option<()> {
    let role = ProtoRole::from_conn(&conn)?;

    match role {
        ProtoRole::Resolver => {
            todo!("SUPPORT: resolver/1")
        },
        ProtoRole::Client => {
            todo!("SUPPORT: client/1")
        },
        ProtoRole::Node => {
            todo!("SUPPORT: node/1")
        },
        _ => conn.close(0u32.into(), b"UnsupportedALPN"),
    }

    Some(())
}
