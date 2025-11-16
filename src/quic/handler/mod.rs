mod client;
mod relay;
mod resolver;

use std::sync::Arc;

use common::quic::protorole::ProtoRole;
use quinn::Connection;
use relay::HandleRelay;

use crate::quic::handler::client::HandleClient;
use crate::quic::handler::resolver::HandleResolver;
use crate::resolver::ResolverRef;
use crate::ret;

pub struct Handler {
    conn: Arc<Connection>,
}

impl Handler {
    pub async fn handle(conn: Connection, resolver: ResolverRef) {
        let role = ret!(ProtoRole::from_conn(&conn));

        let handler = Self { conn: Arc::new(conn) };

        match role {
            ProtoRole::Resolver => handler.handle_resolver(resolver).await,
            ProtoRole::Client => handler.handle_client(resolver).await,
            ProtoRole::Relay => handler.handle_relay(resolver).await,
            _ => handler.conn.close(0u32.into(), b"UnsupportedALPN"),
        };
    }
}
