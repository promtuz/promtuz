use std::sync::Arc;

use common::crypto::PublicKey;
use common::crypto::get_nonce;
use common::debug;
use common::proto::client_rel::RelayPacket;
use common::proto::pack::Unpacker;
use quinn::Connection;
use tokio::sync::RwLock;

use crate::quic::handler::Handler;
use crate::quic::handler::client::events::handle_packet;
use crate::relay::RelayRef;

mod events;

/// Context for client connection
pub struct ClientContext {
    /// Random bytes used for signature based handshake authentication
    pub nonce: [u8; 32],
    pub ipk: Option<PublicKey>,
    pub relay: RelayRef,
    // pub send: Option<SendStream>,
    pub conn: Arc<Connection>,
}

pub type ClientCtxHandle = Arc<RwLock<ClientContext>>;

impl Handler {
    pub async fn handle_client(self, relay: RelayRef) {
        let conn = self.conn.clone();
        let addr = self.conn.remote_address();

        debug!("incoming conn from client({addr})");

        let ctx = Arc::new(RwLock::new(ClientContext {
            nonce: get_nonce(),
            ipk: None,
            relay: relay.clone(),
            // send: None,
            conn: conn.clone(),
        }));

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                while let Ok(packet) = RelayPacket::unpack(&mut recv).await {
                    handle_packet(packet, ctx.clone(), &mut send).await.ok()?;
                }
                Some(())
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            debug!("conn client({addr}) closed: {close_reason}");
        }

        // Deregister client on disconnect
        let ipk = ctx.read().await.ipk.map(|k| k.to_bytes());
        if let Some(ipk) = ipk {
            relay.lock().await.clients.remove(&ipk);
        }
    }
}
