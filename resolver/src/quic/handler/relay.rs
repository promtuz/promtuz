use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub(super) trait HandleRelay {
    async fn handle_relay(self, resolver: ResolverRef);
}

impl HandleRelay for Handler {
    async fn handle_relay(self, resolver: ResolverRef) {
        loop {
            let mut recv = match self.conn.accept_uni().await {
                Ok(recv) => recv,
                Err(err) => {
                    println!("RELAY_CLOSE: {err}");
                    break;
                },
            };

            let conn = self.conn.clone();
            let resolver = resolver.clone();

            tokio::spawn(async move {
                while let Ok(packet) = ResolverPacket::unpack(&mut recv).await {
                    if let Err(e) = handle_packet(conn.clone(), resolver.clone(), packet).await {
                        eprintln!("Packet handling error: {e}");
                    }
                }
            });
        }
    }
}

async fn handle_packet(
    conn: Arc<Connection>, resolver: ResolverRef, packet: ResolverPacket,
) -> Result<()> {
    use ResolverPacket::*;
    match packet {
        Lifetime(liftime) => handle_lifetime(conn.clone(), resolver.clone(), liftime).await,
        // _ => Err(anyhow!("No!")),
    }
}

async fn handle_lifetime(
    conn: Arc<Connection>, resolver: ResolverRef, packet: LifetimeP,
) -> Result<()> {
    use LifetimeP::*;
    match packet {
        hello @ RelayHello { .. } => {
            let hello_ack = match resolver.lock().await.register_relay(conn.clone(), &hello) {
                Ok(ack) => ResolverPacket::Lifetime(ack),
                Err(close) => {
                    return {
                        close.close(&conn);
                        Err(anyhow!("closed"))
                    };
                },
            };

            let mut send = conn.open_uni().await?;
            hello_ack.send(&mut send).await?;
            send.finish()?;

            Ok(())
        },
        RelayHeartbeat { .. } => Ok(()),
        _ => Err(anyhow!("No!")),
    }
}
