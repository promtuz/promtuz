pub(crate) mod client;
pub(crate) mod peer;
mod resolver;

use common::quic::protorole::ProtoRole;
use common::ret;
use quinn::Connection;
use tokio_util::sync::CancellationToken;

use crate::relay::RelayRef;

pub struct Handler {
    conn: Connection,
}

impl Handler {
    /// Handles **incoming** connection. The `cancel` token is observed by
    /// long-running per-role loops so a Ctrl-C in `main.rs` can wind them
    /// down cooperatively rather than killing them mid-fjall-batch.
    pub async fn handle(conn: Connection, relay: RelayRef, cancel: CancellationToken) {
        let role = ret!(ProtoRole::from_conn(&conn));

        let handler = Self { conn };

        match role {
            ProtoRole::Resolver => handler.handle_resolver(relay).await,
            ProtoRole::Client => handler.handle_client(relay, cancel).await,
            ProtoRole::Peer => handler.handle_peer(relay).await,
            _ => handler.conn.close(0u32.into(), b"UnsupportedALPN"),
        };
    }
}
