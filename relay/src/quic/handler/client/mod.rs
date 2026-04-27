use std::sync::Arc;

use common::crypto::PublicKey;
use common::debug;
use common::proto::client_rel::CRelayPacket;
use common::proto::pack::Unpacker;
use common::warn;
use parking_lot::Mutex;
use quinn::Connection;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::storage::MessageKey;

use crate::quic::handler::Handler;
use crate::quic::handler::client::events::handle_packet;
use crate::quic::handler::client::handshake::handle_handshake;
use crate::relay::RelayRef;

mod events;
mod handshake;

/// Context for client connection
pub struct ClientContext {
    pub ipk: PublicKey,
    pub relay: RelayRef,
    pub conn: Connection,
    /// Keys delivered in the most recent `DrainQueue` whose `AckDrain` we are
    /// still waiting for. Cleared *only* on `AckDrain` so that a re-drain
    /// before the ack lands re-sends the same set rather than dropping it.
    pub pending_drain: Mutex<Vec<MessageKey>>,
}

pub type ClientCtxHandle = Arc<ClientContext>;

/// Remove the entry for `ipk` only if its `Connection` is the same one we
/// own (compared by `stable_id()`). This prevents a stale cleanup task from
/// wiping a freshly-registered re-handshake's entry.
///
/// `Connection` is internally an `Arc`, so cloning is cheap, but it does
/// not expose its inner pointer for `Arc::ptr_eq`. `stable_id()` returns a
/// per-connection unique id that survives clones — equivalent guarantee.
pub(crate) fn remove_client_if_same(relay: &RelayRef, ipk: &[u8; 32], owned: &Connection) {
    let mut clients = relay.clients.write();
    let same = clients
        .get(ipk)
        .map(|c| c.stable_id() == owned.stable_id())
        .unwrap_or(false);
    if same {
        clients.remove(ipk);
    }
}

impl Handler {
    pub async fn handle_client(self, relay: RelayRef, cancel: CancellationToken) {
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

        let context = Arc::new(ClientContext {
            ipk,
            relay: relay.clone(),
            conn: conn.clone(),
            pending_drain: Mutex::new(Vec::new()),
        });

        // only 16 concurrent streams can run at once per connection
        let limiter = Arc::new(Semaphore::new(16));

        loop {
            let accept = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!("client({addr}) loop cancelled by shutdown");
                    break;
                }
                accept = conn.accept_bi() => accept,
            };
            let (mut send, mut recv) = match accept {
                Ok(s) => s,
                Err(_) => break,
            };

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

                while let Ok(packet) = CRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = handle_packet(packet, context.clone(), &mut send).await {
                        warn!("client({addr}) packet handler failed: {err}");
                    }
                }
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            debug!("conn client({addr}) closed: {close_reason}");
        }

        // Deregister client on disconnect — but only if the entry still
        // points at *our* connection. A re-handshake for the same IPK that
        // raced past our `accept_bi` failure would have already replaced
        // the entry; in that case we must leave it alone.
        remove_client_if_same(&relay, ipk.as_bytes(), &self.conn);
    }
}
