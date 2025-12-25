use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::Ordering;

use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::encrypt::Encrypted;
use common::crypto::get_shared_key;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::relay::HandshakePacket;
use common::msg::relay::MiscPacket;
use log::debug;
use log::error;
use log::info;
use parking_lot::RwLock;
use quinn::ConnectionError;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

use crate::ENDPOINT;
use crate::api::conn_stats::CONNECTION_START_TIME;
use crate::data::relay::Relay;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;

pub enum RelayConnError {
    Continue,
    Error(anyhow::Error),
}

impl<E> From<E> for RelayConnError
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(err: E) -> Self {
        RelayConnError::Error(err.into())
    }
}

pub struct KeyPair {
    pub public: common::crypto::PublicKey,
    pub secret: common::crypto::StaticSecret,
}

pub static RELAY: RwLock<Option<Relay>> = RwLock::new(None);

impl Relay {
    pub async fn connect(mut self, keypair: &KeyPair) -> Result<(), RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host.clone())?, self.port);

        info!("RELAY({}): CONNECTING AT {}", self.id, addr);

        ConnectionState::Connecting.emit();

        match ENDPOINT.get().unwrap().connect(addr, &self.id)?.await {
            Ok(conn) => {
                info!("RELAY({}): Connected", self.id);

                ConnectionState::Handshaking.emit();

                let (mut send, mut recv) = conn.open_bi().await?;

                use HandshakePacket::*;

                let client_hello = ClientHello { ipk: keypair.public.to_bytes() }.pack().unwrap();
                send.write_all(&client_hello).await?;
                send.flush().await?;

                debug!("RELAY({}): SENDING {}", self.id, hex::encode(client_hello));

                // let conn = Arc::new(conn;
                loop {
                    let mut packet = vec![0u8; recv.read_u32().await? as usize];
                    recv.read_exact(&mut packet).await?;

                    let msg = HandshakePacket::from_cbor(&packet).map_err(RelayConnError::Error)?;

                    debug!("RELAY({}): RECEIVED {:?}", self.id, msg);

                    if let ServerChallenge { ct, epk } = msg {
                        let secret = keypair.secret.diffie_hellman(&PublicKey::from(epk));

                        let key = get_shared_key(
                            secret.as_bytes(),
                            &[0u8; 32],
                            "handshake.challenge.key",
                        );

                        // Authenticated Data
                        let ad = &[&keypair.public.as_bytes()[..], &epk[..]].concat();

                        let encrypted = Encrypted { cipher: ct.to_vec(), nonce: vec![0u8; 12] };

                        let proof = encrypted.decrypt(&key, ad)?.try_into().map_err(|_| {
                            RelayConnError::Error(anyhow!("server proof is invalid"))
                        })?;

                        debug!("RELAY({}): DECRYPTED PROOF - {}", self.id, hex::encode(proof));

                        let client_proof =
                            ClientProof { proof }.pack().map_err(RelayConnError::Error)?;

                        debug!("RELAY({}): SENDING {}", self.id, hex::encode(&client_proof));
                        send.write_all(&client_proof).await?;
                        send.flush().await?;
                    } else if let ServerAccept { timestamp } = msg {
                        info!("RELAY({}): Authenticated at {timestamp}", self.id);

                        CONNECTION_START_TIME.store(timestamp, Ordering::Relaxed);

                        // Informing App UI about conn status
                        ConnectionState::Connected.emit();

                        // Respect++
                        self.upvote().map_err(RelayConnError::Error)?;

                        self.connection = Some(conn);
                        *RELAY.write() = Some(self);

                        // Starting the handler, handles until it's connected
                        Self::handle();

                        return Ok(());
                    } else if let ServerReject { reason } = msg {
                        info!("RELAY({}): Rejected because {reason}", self.id);

                        // or something else maybe
                        return Err(RelayConnError::Continue);
                    }
                }
            },
            Err(ConnectionError::TimedOut) => {
                ConnectionState::Failed.emit();

                _ = self.downvote();

                Err(RelayConnError::Continue)
            },
            Err(err) => {
                debug!("RELAY({}): Connection Fail because {:?}", self.id, err);
                Err(err.into())
            },
        }
    }

    /// fetches public address
    pub async fn public_addr(&self) -> Option<IpAddr> {
        let conn = self.connection.as_ref()?;
        let (mut tx, mut rx) = conn.open_bi().await.ok()?;

        tx.write_all(&MiscPacket::PubAddressReq.pack().ok()?).await.ok()?;
        tx.flush().await.ok()?;

        let len = rx.read_u32().await.ok()? as usize;
        let mut packet = vec![0; len];
        rx.read_exact(&mut packet).await.ok()?;

        match MiscPacket::from_cbor(&packet).ok()? {
            MiscPacket::PubAddressRes { addr } => Some(addr),
            _ => None,
        }
    }

    fn handle() {
        tokio::spawn(async {
            debug!("RELAY_HANDLE: STANDBY");
            loop {
                let conn = {
                    let guard = RELAY.read();
                    guard.as_ref().map(|r| r.connection.clone())
                };

                if let Some(Some(conn)) = conn {
                    match conn.accept_bi().await {
                        Ok((_, _)) => {
                            // Server will not send client anything, YET
                        },
                        Err(err) => {
                            ConnectionState::Disconnected.emit();

                            // cleanup
                            *RELAY.write() = None;

                            return error!("RELAY_HANDLE: {err}");
                        },
                    };
                } else {
                    debug!("RELAY_HANDLE: NO CONN, BYE!");

                    break;
                }
            }
        });
    }
}
