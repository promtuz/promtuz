use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::client_rel::ForwardP;
use common::proto::client_rel::HandshakeP;
use common::proto::client_rel::MiscP;
use common::proto::client_rel::RelayPacket;
use common::proto::pack::Unpacker;
use common::proto::pack::unpack;
use ed25519_dalek::VerifyingKey;
use log::debug;
use log::error;
use log::info;
use log::warn;
use parking_lot::RwLock;
use quinn::Connection;
use quinn::ConnectionError;
use tokio::task::JoinHandle;

use crate::ENDPOINT;
use crate::api::conn_stats::CONNECTION_START_TIME;
use crate::api::messaging::decode_encrypted;
use crate::data::contact::Contact;
use crate::data::identity::IdentitySigner;
use crate::data::relay::Relay;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::events::messaging::MessageEv;
use crate::utils::systime;

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
    pub async fn connect(
        mut self, ipk: VerifyingKey, signer: &IdentitySigner,
    ) -> Result<JoinHandle<ConnectionError>, RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host.clone())?, self.port);

        debug!("DEBUG: connecting to relay at {}", addr);

        ConnectionState::Connecting.emit();

        match ENDPOINT.get().unwrap().connect(addr, &self.id)?.await {
            Ok(conn) => {
                info!("INFO: relay({}) connected", self.id);

                ConnectionState::Handshaking.emit();

                let (mut send, mut recv) = conn.open_bi().await?;

                use HandshakeP::*;
                use RelayPacket::*;

                RelayPacket::Handshake(ClientHello { ipk: ipk.to_bytes() })
                    .send(&mut send)
                    .await
                    .map_err(RelayConnError::Error)?;

                loop {
                    match RelayPacket::unpack(&mut recv).await.map_err(RelayConnError::Error)? {
                        Handshake(ServerChallenge { nonce }) => {
                            let msg =
                                [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &nonce]
                                    .concat();

                            RelayPacket::Handshake(ClientProof {
                                sig: signer.sign(&msg).map_err(RelayConnError::Error)?.to_bytes(),
                            })
                            .send(&mut send)
                            .await
                            .map_err(RelayConnError::Error)?;
                        },
                        Handshake(ServerAccept { timestamp }) => {
                            info!("INFO: authenticated with relay({}) at {timestamp}", self.id);

                            CONNECTION_START_TIME.store(timestamp, Ordering::Relaxed);
                            ConnectionState::Connected.emit();

                            self.upvote().map_err(RelayConnError::Error)?;
                            self.connection = Some(conn.clone());

                            let handle = tokio::spawn({
                                let relay_clone = self.clone();
                                async move { relay_clone.handle(conn).await }
                            });

                            *RELAY.write() = Some(self);

                            return Ok(handle);
                        },
                        Handshake(ServerReject { reason }) => {
                            error!("ERROR: handshake with relay({}) rejected: {reason}", self.id);
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
                error!("ERROR: connection with relay({}) failed: {}", self.id, err);
                Err(err.into())
            },
        }
    }

    /// fetches public address
    pub async fn public_addr(&self) -> Result<SocketAddr> {
        let conn = self.connection.as_ref().ok_or(anyhow!("relay not connected"))?;
        let (mut tx, mut rx) =
            conn.open_bi().await.map_err(|e| anyhow!("failed to open stream: {e}"))?;

        RelayPacket::Misc(MiscP::PubAddressReq).send(&mut tx).await?;

        tx.finish()?;

        match unpack(&mut rx).await.map_err(|e| anyhow!("failed to unpack packet: {e}"))? {
            RelayPacket::Misc(MiscP::PubAddressRes { addr }) => Ok(addr),
            unknown => Err(anyhow!("got unknown response: {unknown:?}")),
        }
    }

    /// keep waiting for new incoming streams
    /// IF,
    /// - [`Relay::connection`] is not empty
    /// - [`Relay::connection`] is not closed
    async fn handle(&self, conn: Connection) -> ConnectionError {
        debug!("DEBUG: waiting for incoming streams");
        loop {
            match conn.accept_bi().await {
                Ok((_, mut recv)) => {
                    tokio::spawn(async move {
                        while let Ok(packet) = RelayPacket::unpack(&mut recv).await {
                            match packet {
                                RelayPacket::Deliver(fwd) => {
                                    handle_deliver(fwd);
                                },
                                other => {
                                    debug!("DEBUG: unexpected packet from relay: {other:?}");
                                },
                            }
                        }
                    });
                },
                Err(err) => {
                    ConnectionState::Disconnected.emit();

                    // cleanup
                    *RELAY.write() = None;
                    error!("ERROR: failed to accept stream: {err}");

                    return err;
                },
            };
        }
    }
}

fn handle_deliver(fwd: ForwardP) {
    // 1. Check if sender is a known contact
    let contact = match Contact::get(&fwd.from) {
        Some(c) => c,
        None => {
            info!("MESSAGE: dropped message from unknown sender {}", hex::encode(fwd.from));
            return;
        },
    };

    // 2. Derive per-friendship shared key and decrypt
    let shared_key = match contact.shared_key() {
        Ok(k) => k,
        Err(e) => {
            warn!("MESSAGE: failed to derive shared key: {e}");
            return;
        },
    };

    let encrypted = match decode_encrypted(&fwd.payload) {
        Some(e) => e,
        None => {
            warn!("MESSAGE: payload too short from {}", hex::encode(fwd.from));
            return;
        },
    };

    let plaintext = match encrypted.decrypt(&shared_key, &fwd.to) {
        Ok(p) => p,
        Err(_) => {
            warn!("MESSAGE: decryption failed from {}", hex::encode(fwd.from));
            return;
        },
    };

    let content = match String::from_utf8(plaintext) {
        Ok(s) => s,
        Err(_) => {
            warn!("MESSAGE: invalid UTF-8 from {}", hex::encode(fwd.from));
            return;
        },
    };

    info!("MESSAGE: received from {}", hex::encode(fwd.from));

    MessageEv::Received {
        from: fwd.from,
        content,
        timestamp: systime().as_secs(),
    }
    .emit();
}
