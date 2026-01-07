use std::sync::Arc;

use common::crypto::PublicKey;
use common::crypto::get_nonce;
use quinn::Connection;
use quinn::SendStream;
use tokio::io::AsyncReadExt;
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
    pub send: Option<SendStream>,
    pub conn: Arc<Connection>,
}

pub type ClientCtxHandle = Arc<RwLock<ClientContext>>;

impl Handler {
    pub async fn handle_client(self, relay: RelayRef) {
        let conn = self.conn.clone();

        println!("CLIENT: CONN({})", self.conn.remote_address());

        let ctx = Arc::new(RwLock::new(ClientContext {
            nonce: get_nonce(),
            ipk: None,
            relay: relay.clone(),
            send: None,
            conn: conn.clone(),
        }));

        while let Ok((send, mut recv)) = conn.accept_bi().await {
            ctx.write().await.send = Some(send);

            let ctx = ctx.clone();
            tokio::spawn(async move {
                while let Ok(packet_size) = recv.read_u32().await {
                    let mut packet = vec![0u8; packet_size as usize];
                    if let Err(_err) = recv.read_exact(&mut packet).await {
                        break;
                    }

                    handle_packet(&packet, ctx.clone()).await.ok()?;
                }
                Some(())
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            println!("CLIENT({}): CLOSE({})", self.conn.remote_address(), close_reason);
        }
    }
}
