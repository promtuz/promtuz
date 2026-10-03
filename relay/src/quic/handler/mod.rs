pub(crate) mod client;
pub(crate) mod peer;

use common::quic::CloseReason;
use common::quic::protorole::ProtoRole;
use common::ret;
use quinn::Connection;
use tokio_util::sync::CancellationToken;

use crate::relay::RelayRef;

pub struct Handler {
    conn: Connection,
}

impl Handler {
    pub async fn handle(conn: Connection, relay: RelayRef, cancel: CancellationToken) {
        let role = ret!(ProtoRole::from_conn(&conn));

        let handler = Self { conn };

        match role {
            ProtoRole::Client => handler.handle_client(relay, cancel).await,
            ProtoRole::Peer => handler.handle_peer(relay).await,
            _ => CloseReason::UnsupportedRole.close(&handler.conn),
        };
    }
}
