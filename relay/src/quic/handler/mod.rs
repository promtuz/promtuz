mod client;
pub(crate) mod peer;
mod resolver;

use std::sync::Arc;

use common::quic::protorole::ProtoRole;
use common::ret;
use quinn::Connection;

use crate::relay::RelayRef;

pub struct Handler {
    conn: Arc<Connection>,
}

impl Handler {
    /// Handles **incoming** connection
    pub async fn handle(conn: Connection, relay: RelayRef) {
        let role = ret!(ProtoRole::from_conn(&conn));

        let handler = Self { conn: Arc::new(conn) };

        match role {
            ProtoRole::Resolver => handler.handle_resolver(relay).await,
            ProtoRole::Client => handler.handle_client(relay).await,
            ProtoRole::Peer => handler.handle_peer(relay).await,
            _ => handler.conn.close(0u32.into(), b"UnsupportedALPN"),
        };
    }
}
