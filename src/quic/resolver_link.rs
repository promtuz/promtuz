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
use quinn::TransportConfig;
use tokio::io::AsyncWriteExt;
use tokio::sync::watch::Receiver;
use tokio::task::JoinHandle;

use crate::quic::dialer::connect_to_any_seed;
use crate::relay::RelayRef;
use crate::util::systime;
use crate::util::systime_sec;

pub struct ResolverLink {
    relay: RelayRef,
    conn: Arc<Connection>,
    shutdown: Receiver<()>,
}

impl ResolverLink {
    /// Transport config for `Relay <-> Resolver`
    fn transport_cfg() -> Arc<TransportConfig> {
        let mut cfg = TransportConfig::default();

        cfg.keep_alive_interval(Some(Duration::from_secs(15)));

        Arc::new(cfg)
    }

    async fn id(&self) -> NodeId {
        self.relay.lock().await.id
    }

    /// Attaches with relay, it's job is to keep in contact with any resolver however possible.
    ///
    /// * `relay` - reference of relay to attach with
    /// * `rx` - shutdown receiver, used for closing the loop
    pub async fn attach(relay: RelayRef, rx: Receiver<()>) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let conn: Connection = {
                let relay = relay.lock().await;

                let mut cfg = (*relay.client_cfg).clone();
                cfg.transport_config(Self::transport_cfg());

                connect_to_any_seed(&relay.endpoint, &relay.cfg.resolver.seed, Some(cfg)).await?
            };

            let mut resolver = Self { relay, conn: Arc::new(conn), shutdown: rx };

            // Announcing Presence to Resolver
            resolver.hello().await?;

            let _ = resolver.handle().await;

            Ok(())
        })
    }

    /// Sends node status to current resolver
    ///
    /// * `allow(unused)` - will use in future
    #[allow(unused)]
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
                    println!("HEARTBEAT: RTT({}ms)", conn.rtt().as_millis());

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
        let hello =
            RelayHello { relay_id: self.id().await, timestamp: systime().as_millis() }.to_cbor()?;

        let mut send = self.conn.open_uni().await?;
        send.write_all(&hello).await?;
        send.finish()?;

        println!("SENT: RelayHello");

        Ok(())
    }

    pub async fn handle(&mut self) -> Result<()> {
        let conn = self.conn.clone();
        loop {
            let mut recv = tokio::select! {
                _ = self.shutdown.changed() => {
                    self.close();
                    break Ok(())
                },
                res = conn.accept_uni() => res?,
            };

            let packet = recv.read_to_end(4096).await?;
            if let Ok(ack) = HelloAck::from_cbor(&packet) {
                println!("RECV_ACK: {ack:?}");
                continue;
            }
            tokio::spawn(async move {
                println!("RECV_PACKET: {packet:?}");
                Some(())
            });
        }
    }

    pub fn close(&self) {
        self.conn.close(CloseReason::ShuttingDown.code(), b"RelayShuttingDown");
    }
}
