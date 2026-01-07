use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::Ordering;

use common::PROTOCOL_VERSION;
use common::crypto::SigningKey;
use common::msg::pack::Unpacker;
use common::msg::relay::HandshakeP;
use common::msg::relay::MiscP;
use common::msg::relay::RelayPacket;
use ed25519_dalek::ed25519::signature::SignerMut;
use log::debug;
use log::error;
use log::info;
use parking_lot::RwLock;
use quinn::ConnectionError;
use tokio::io::AsyncReadExt;

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

pub static RELAY: RwLock<Option<Relay>> = RwLock::new(None);

impl Relay {
    pub async fn connect(mut self, mut isk: SigningKey) -> Result<(), RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host.clone())?, self.port);

        info!("RELAY({}): CONNECTING AT {}", self.id, addr);

        ConnectionState::Connecting.emit();

        match ENDPOINT.get().unwrap().connect(addr, &self.id)?.await {
            Ok(conn) => {
                info!("RELAY({}): Connected", self.id);

                ConnectionState::Handshaking.emit();

                let (mut send, mut recv) = conn.open_bi().await?;

                use HandshakeP::*;
                use RelayPacket::*;

                RelayPacket::Handshake(ClientHello { ipk: isk.verifying_key().to_bytes() })
                    .send(&mut send)
                    .await
                    .map_err(RelayConnError::Error)?;

                loop {
                    let mut packet = vec![0u8; recv.read_u32().await? as usize];
                    recv.read_exact(&mut packet).await?;

                    let msg = RelayPacket::from_cbor(&packet).map_err(RelayConnError::Error)?;

                    match msg {
                        Handshake(ServerChallenge { nonce }) => {
                            let msg =
                                [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &nonce]
                                    .concat();

                            RelayPacket::Handshake(ClientProof { sig: isk.sign(&msg).to_bytes() })
                                .send(&mut send)
                                .await
                                .map_err(RelayConnError::Error)?;
                        },
                        Handshake(ServerAccept { timestamp }) => {
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
                        },
                        Handshake(ServerReject { reason }) => {
                            info!("RELAY({}): Rejected because {reason}", self.id);
                            // or something else maybe
                            return Err(RelayConnError::Continue);
                        },
                        _ => {},
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

        RelayPacket::Misc(MiscP::PubAddressReq).send(&mut tx).await.ok()?;

        match RelayPacket::unpack(&mut rx).await.ok()? {
            RelayPacket::Misc(MiscP::PubAddressRes { addr }) => Some(addr),
            _ => None
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
