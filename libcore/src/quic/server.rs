use std::io;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::Sender;
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
use quinn::ConnectionError;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::ENDPOINT;
use crate::api::conn_stats::CONNECTION_START_TIME;
use crate::api::messaging::decode_encrypted;
use crate::data::contact::Contact;
use crate::data::identity::IdentitySigner;
use crate::data::message::Message;
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

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: usize = 64;

pub static RELAY: RwLock<Option<Relay>> = RwLock::new(None);

impl Relay {
    pub async fn connect(
        mut self, ipk: VerifyingKey,
    ) -> Result<JoinHandle<ConnectionError>, RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host)?, self.port);

        debug!("connecting to relay at {}", addr);
        ConnectionState::Connecting.emit();

        let connect_start = systime().as_millis() as u64;

        let conn = match ENDPOINT.get().unwrap().connect(addr, &self.id)?.await {
            Ok(conn) => conn,
            Err(ConnectionError::TimedOut) => {
                ConnectionState::Failed.emit();
                _ = self.record_failure();
                return Err(RelayConnError::Continue);
            },
            Err(err) => {
                error!("connection with relay({}) failed: {}", self.id, err);
                _ = self.record_failure();
                return Err(err.into());
            },
        };

        ConnectionState::Handshaking.emit();

        let (mut send, mut recv) = conn.open_bi().await?;

        RelayPacket::Handshake(HandshakeP::ClientHello { ipk: ipk.to_bytes() })
            .send(&mut send)
            .await?;
        // TODO: flatten out handshake
        let handshake = timeout(HANDSHAKE_TIMEOUT, async {
            loop {
                match RelayPacket::unpack(&mut recv).await? {
                    RelayPacket::Handshake(HandshakeP::ServerChallenge { nonce }) => {
                        let msg =
                            [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &nonce]
                                .concat();

                        RelayPacket::Handshake(HandshakeP::ClientProof {
                            sig: IdentitySigner::sign(&msg)
                                .map_err(RelayConnError::Error)?
                                .to_bytes(),
                        })
                        .send(&mut send)
                        .await?;
                    },
                    RelayPacket::Handshake(HandshakeP::ServerAccept { timestamp }) => {
                        let latency_ms = systime().as_millis() as u64 - connect_start;
                        break Ok((timestamp, latency_ms));
                    },
                    RelayPacket::Handshake(HandshakeP::ServerReject { reason }) => {
                        error!("handshake with relay({}) rejected: {reason}", self.id);
                        break Err(RelayConnError::Continue);
                    },
                    unexpected => {
                        error!(
                            "unexpected packet during handshake with relay({}): {unexpected:?}",
                            self.id
                        );
                        break Err(RelayConnError::Continue);
                    },
                }
            }
        })
        .await;

        let (timestamp, latency_ms) = match handshake {
            Ok(Ok(inner)) => inner,
            Ok(Err(e)) => {
                _ = self.record_failure();
                return Err(e);
            },
            Err(_elapsed) => {
                _ = self.record_failure();
                return Err(RelayConnError::Continue);
            },
        };

        info!("authenticated with relay({}) at {timestamp}", self.id);
        CONNECTION_START_TIME.store(timestamp, Ordering::Relaxed);
        ConnectionState::Connected.emit();

        self.record_success(latency_ms).map_err(|e| RelayConnError::Error(e.into()))?;
        self.connection = Some(conn);

        let handle = tokio::spawn({
            let relay = self.clone();
            async move { relay.handle().await }
        });

        *RELAY.write() = Some(self);

        Ok(handle)
    }

    /// Waits for incoming streams. Runs until the connection is lost.
    async fn handle(&self) -> ConnectionError {
        let conn = self.connection.as_ref().expect("handle called without active connection");
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS));

        debug!("waiting for incoming streams from relay({})", self.id);

        loop {
            match conn.accept_bi().await {
                Ok((_, mut recv)) => {
                    let permit = match semaphore.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            debug!("relay({}) stream limit reached, dropping stream", self.id);
                            continue;
                        },
                    };

                    tokio::spawn(async move {
                        let _permit = permit; // dropped when stream task ends
                        while let Ok(packet) = RelayPacket::unpack(&mut recv).await {
                            match packet {
                                RelayPacket::Deliver(fwd) => handle_deliver(fwd),
                                other => debug!("unexpected packet from relay: {other:?}"),
                            }
                        }
                    });
                },
                Err(err) => {
                    ConnectionState::Disconnected.emit();
                    _ = self.record_failure();

                    // Only clear RELAY if it still points to this relay.
                    // A reconnect may have already replaced it.
                    let mut guard = RELAY.write();
                    if guard.as_ref().map(|r| r.id == self.id).unwrap_or(false) {
                        *guard = None;
                    }

                    error!("relay({}) connection lost: {err}", self.id);
                    return err;
                },
            }
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
}

fn handle_deliver(fwd: ForwardP) {
    // 1. Check if sender is a known contact
    let Some(contact) = Contact::get(&fwd.from) else {
        info!("MESSAGE: dropped message from unknown sender {}", hex::encode(fwd.from));
        return;
    };

    // 2. Derive per-friendship shared key and decrypt
    let Ok(shared_key) =
        contact.shared_key().inspect_err(|e| warn!("MESSAGE: failed to derive shared key: {e}"))
    else {
        return;
    };

    let Some(encrypted) = decode_encrypted(&fwd.payload) else {
        warn!("MESSAGE: payload too short from {}", hex::encode(fwd.from));
        return;
    };

    let Ok(plaintext) = encrypted.decrypt(&shared_key, &fwd.to) else {
        warn!("MESSAGE: decryption failed from {}", hex::encode(fwd.from));
        return;
    };

    let Ok(content) = String::from_utf8(plaintext) else {
        warn!("MESSAGE: invalid UTF-8 from {}", hex::encode(fwd.from));
        return;
    };

    let timestamp = systime().as_secs();

    info!("MESSAGE: received from {}", hex::encode(fwd.from));

    // 3. Save to local DB
    match Message::save_incoming(fwd.from, &content, timestamp) {
        Ok(m) => {
            MessageEv::Received { id: m.inner.id, from: fwd.from, content, timestamp }.emit();
        },
        Err(e) => {
            warn!("MESSAGE: failed to save incoming: {e}");
        },
    };
}
