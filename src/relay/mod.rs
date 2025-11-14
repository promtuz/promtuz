use std::fs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::resolver::HelloAck;
use common::msg::resolver::RelayHeartbeat;
use common::msg::resolver::RelayHello;
use common::quic::id::derive_id;
use common::sysutils::system_load;
use p256::SecretKey;
use quinn::Connection;
use quinn::Endpoint;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::quic::dialer::connect_to_any_seed;
use crate::util::config::AppConfig;
use crate::util::systime_sec;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub secret: p256::SecretKey,
    pub public: p256::PublicKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self> {
        let sec = fs::read_to_string(&cfg.network.key_path)?;
        let secret = SecretKey::from_sec1_pem(&sec)?;

        Ok(Self { public: secret.public_key(), secret })
    }
}

/// Represents a single relay node running in the network.
///
/// It's *local identity* of the relay process,
/// not a message exchanged over the wire.
///
/// It's apparently like a core process handler
#[derive(Debug)]
pub struct Relay {
    /// Human readable relay id derived from public key
    pub id: String,

    pub keys: RelayKeys,

    /// SystemTime in ms since EPOCH when relay is started first
    pub start_ms: u64,

    /// Connection with one of resolver from given seed
    pub resolver_conn: Arc<Connection>,
}

impl Relay {
    pub async fn init(cfg: &AppConfig, endpoint: Endpoint) -> Result<Self> {
        let keys = RelayKeys::from_cfg(cfg)?;
        let id = derive_id(&keys.public);

        println!("RELAY: Going online with ID({})", id);

        let resolver_conn =
            connect_to_any_seed(&endpoint, &cfg.resolver.seed, "arch.local").await?;

        Ok(Self { id, keys, start_ms: systime_sec(), resolver_conn: Arc::new(resolver_conn) })
    }

    async fn start_heartbeat(&self, ack: HelloAck) -> Result<()> {
        let conn = self.resolver_conn.clone();
        let id = self.id.clone();
        let start_ms = self.start_ms;

        let interval = ack.interval_heartbeat_ms as u64;

        tokio::spawn(async move {
            let mut send = match conn.open_uni().await {
                Ok(s) => s,
                Err(_) => return,
            };
            loop {
                if let Ok(heartbeat) = (RelayHeartbeat {
                    node_id: id.clone(),
                    load: system_load().await,
                    uptime_seconds: systime_sec() - start_ms,
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
        let hello = RelayHello { relay_id: self.id.clone() }.to_cbor()?;

        let mut send = self.resolver_conn.open_uni().await?;
        send.write_all(&hello).await?;
        send.finish()?;
        println!("SENT: RelayHello");

        Ok(())
    }

    pub async fn handle_resolver(&self) -> Result<()> {
        let conn = self.resolver_conn.clone();
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
                    return Err(err.into());
                },
            }
        }
    }
}
