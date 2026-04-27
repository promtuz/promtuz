use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::debug;
use common::error;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::warn;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub(super) trait HandleRelay {
    async fn handle_relay(self, resolver: ResolverRef);
}

impl HandleRelay for Handler {
    async fn handle_relay(self, resolver: ResolverRef) {
        let addr = self.conn.remote_address();
        loop {
            let mut recv = match self.conn.accept_uni().await {
                Ok(recv) => recv,
                Err(err) => {
                    debug!("relay({addr}) stream accept ended: {err}");
                    break;
                },
            };

            let conn = self.conn.clone();
            let resolver = resolver.clone();

            tokio::spawn(async move {
                while let Ok(packet) = ResolverPacket::unpack(&mut recv).await {
                    match handle_packet(conn.clone(), resolver.clone(), packet).await {
                        Ok(()) => {},
                        // Policy-driven close: we already closed the connection
                        // with a `CloseReason`. Don't re-log it as an error.
                        Err(PacketError::PolicyClose) => return,
                        Err(PacketError::Other(e)) => {
                            error!("relay({addr}) packet handling error: {e}");
                        },
                    }
                }
            });
        }
    }
}

/// Internal classification so policy-driven closes (where the resolver
/// already explained itself with a `CloseReason`) don't masquerade as
/// loud, scary errors in the log.
#[derive(Debug)]
enum PacketError {
    PolicyClose,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for PacketError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

async fn handle_packet(
    conn: Arc<Connection>, resolver: ResolverRef, packet: ResolverPacket,
) -> Result<(), PacketError> {
    use ResolverPacket::*;
    match packet {
        Lifetime(liftime) => handle_lifetime(conn.clone(), resolver.clone(), liftime).await,
    }
}

async fn handle_lifetime(
    conn: Arc<Connection>, resolver: ResolverRef, packet: LifetimeP,
) -> Result<(), PacketError> {
    use LifetimeP::*;
    match packet {
        RelayHello { relay_id, pubkey, timestamp, sig } => {
            // Re-pack into a borrowed view for `register_relay` so we keep
            // a single source of truth for the field layout.
            let hello = RelayHello { relay_id, pubkey, timestamp, sig };

            let hello_ack = match resolver.register_relay(conn.clone(), &hello) {
                Ok(ack) => ResolverPacket::Lifetime(ack),
                Err(close) => {
                    close.close(&conn);
                    // Already closed with an explicit reason — caller should
                    // not log this as a packet handling error.
                    return Err(PacketError::PolicyClose);
                },
            };

            // Now that registration is committed, attach the eviction watcher.
            resolver.watch_relay(relay_id, conn.clone());

            debug!("sending to relay({})", conn.remote_address());

            let mut send = conn.open_uni().await.map_err(anyhow::Error::from)?;
            hello_ack.send(&mut send).await.map_err(anyhow::Error::from)?;
            send.finish().map_err(anyhow::Error::from)?;

            Ok(())
        },
        ref hb @ RelayHeartbeat { .. } => {
            // Per-packet auth so liveness/load metrics can't be spoofed by
            // any peer that happens to know the registered `relay_id`.
            if let Err(close) = resolver.verify_heartbeat(&conn, hb) {
                close.close(&conn);
                return Err(PacketError::PolicyClose);
            }
            // Liveness/load consumption is not implemented yet — once it
            // is, plug it in here. Verification still runs unconditionally
            // so the auth path can't regress quietly.
            Ok(())
        },
        _ => {
            warn!("unexpected lifetime packet from relay({})", conn.remote_address());
            Err(PacketError::Other(anyhow!("unexpected lifetime packet")))
        },
    }
}
