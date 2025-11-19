//! Maintains connection with resolver

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::reason::CloseReason;
use common::msg::resolver::HelloAck;
use common::msg::resolver::RelayHeartbeat;
use common::msg::resolver::RelayHello;
use common::quic::id::NodeId;
use common::sysutils::system_load;
use quinn::Connection;
use tokio::io::AsyncWriteExt;

use crate::quic::dialer::connect_to_any_seed;
use crate::relay::RelayRef;
use crate::util::systime;
use crate::util::systime_sec;

pub struct ResolverLink {
    relay: RelayRef,
    conn: Arc<Connection>,
}

impl ResolverLink {
    async fn id(&self) -> NodeId {
        self.relay.lock().await.id
    }

    pub async fn new(relay: RelayRef) -> Result<Self> {
        println!("CREATING RESOLVER LINK");

        let conn = {
            let relay = relay.lock().await;

            connect_to_any_seed(&relay.endpoint, &relay.cfg.resolver.seed).await?
        };

        Ok(Self { relay, conn: Arc::new(conn) })
    }

    async fn start_heartbeat(&self, ack: HelloAck) -> Result<()> {
        let conn = self.conn.clone();
        let id = self.id().await;
        let start_ms = self.relay.lock().await.start_ms;

        let interval = ack.interval_heartbeat_ms as u64;

        tokio::spawn(async move {
            let mut send = match conn.open_uni().await {
                Ok(s) => s,
                Err(_) => return,
            };
            loop {
                if let Ok(heartbeat) = (RelayHeartbeat {
                    relay_id: id,
                    load: system_load().await,
                    uptime_seconds: systime_sec() - ((start_ms / 1000) as u64),
                })
                .to_cbor()
                {
                    if send.write_all(&heartbeat).await.is_err() {
                        break;
                    }
                    if send.flush().await.is_err() {
                        break;
                    }
                }

                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        });

        Ok(())
    }

    pub async fn hello(&self) -> Result<()> {
        let hello = RelayHello { relay_id: self.id().await, timestamp: systime().as_millis() }.to_cbor()?;

        let mut send = self.conn.open_uni().await?;
        send.write_all(&hello).await?;
        send.finish()?;

        println!("SENT: RelayHello");

        Ok(())
    }

    pub async fn handle(&self) -> Result<()> {
        let conn = self.conn.clone();
        loop {
            match conn.accept_uni().await {
                Ok(mut recv) => {
                    let packet = recv.read_to_end(4096).await?;
                    if let Ok(ack) = HelloAck::from_cbor(&packet) {
                        println!("RECV_ACK: {:?}", ack);
                        self.start_heartbeat(ack).await?;
                        continue;
                    }
                    tokio::spawn(async move {
                        println!("RECV_PACKET: {:?}", packet);
                        Some(())
                    });
                },
                Err(err) => {
                    eprintln!("CONN_ERR: {}", err);
                    return Ok(());
                },
            }
        }
    }

    pub fn close(&self) {
        self.conn.close(CloseReason::ShuttingDown.code(), b"RelayShuttingDown");
    }
}
