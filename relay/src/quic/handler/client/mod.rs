use std::sync::Arc;

use common::crypto::PublicKey;
use common::debug;
use common::proto::client_rel::RelayPacket;
use common::proto::pack::Unpacker;
use common::warn;
use quinn::Connection;
use tokio::sync::Semaphore;

use crate::quic::handler::Handler;
use crate::quic::handler::client::events::handle_packet;
use crate::quic::handler::client::events::handshake::handle_handshake;
use crate::relay::RelayRef;

mod events;

/// Context for client connection
pub struct ClientContext {
    pub ipk: PublicKey,
    pub relay: RelayRef,
    pub conn: Connection,
}

pub type ClientCtxHandle = Arc<ClientContext>;

impl Handler {
    pub async fn handle_client(self, relay: RelayRef) {
        let conn = self.conn.clone();
        let addr = self.conn.remote_address();

        debug!("incoming conn from client({addr})");

        let ipk = match handle_handshake(relay.clone(), &conn).await {
            Ok(ipk) => ipk,
            Err(err) => {
                warn!("client({addr}) handshake failed: {err}");
                return;
            },
        };

        let context = Arc::new(ClientContext { ipk, relay: relay.clone(), conn: conn.clone() });

        // only 16 concurrent streams can run at once per connection
        let limiter = Arc::new(Semaphore::new(16));

        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let permit = match limiter.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    // Optional: reject stream politely
                    continue;
                },
            };

            let context = context.clone();
            tokio::spawn(async move {
                let _permit = permit;

                while let Ok(packet) = RelayPacket::unpack(&mut recv).await {
                    if let Err(err) = handle_packet(packet, context.clone(), &mut send).await {
                        warn!("client({addr}) packet handler failed: {err}");
                    }
                }
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            debug!("conn client({addr}) closed: {close_reason}");
        }

        // Deregister client on disconnect
        relay.clients.write().remove(ipk.as_bytes());
    }
}
