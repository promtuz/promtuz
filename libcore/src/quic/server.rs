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
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::QueryP;
use common::proto::client_rel::QueryResultP;
use common::proto::client_rel::SHandshakePacket as SHSP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::ServerHandshakeResultP as SHSRP;
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

        //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

        // 0. Open first bi-stream just for handshake

        let (mut tx, mut rx) = conn.open_bi().await?;

        //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

        // 1. Server is expecting `Hello` from client

        CHandshakePacket::Hello { ipk: ipk.to_bytes().into() }.send(&mut tx).await?;

        //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

        // 2. Server must respond with challenge

        let SHSP::Challenge { nonce } = SHSP::unpack(&mut rx).await? else {
            return Err(RelayConnError::Error(anyhow!("Handshake Packet Order Mismatch")));
        };

        let msg = [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &*nonce].concat();

        CHandshakePacket::Proof {
            sig: IdentitySigner::sign(&msg).map_err(RelayConnError::Error)?.to_bytes().into(),
        }
        .send(&mut tx)
        .await?;

        //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

        // 3. Server either accepts or rejects

        let SHSP::HandshakeResult(result) = SHSP::unpack(&mut rx).await? else {
            return Err(RelayConnError::Error(anyhow!("Handshake Packet Order Mismatch")));
        };

        let (timestamp, latency_ms) = match result {
            SHSRP::Accept { timestamp } => {
                let latency_ms = systime().as_millis() as u64 - connect_start;
                _ = self.record_success(latency_ms);
                (timestamp, latency_ms)
            },
            SHSRP::Reject { reason } => {
                warn!("relay handshake failed : {reason}");
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
            async move { relay.handle(ipk).await }
        });

        *RELAY.write() = Some(self);

        Ok(handle)
    }

    /// Waits for incoming streams. Runs until the connection is lost.
    async fn handle(&self, ipk: VerifyingKey) -> ConnectionError {
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
                        while let Ok(packet) = SRelayPacket::unpack(&mut recv).await {
                            match packet {
                                SRelayPacket::Deliver(msg) => handle_deliver(ipk, msg),
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

        CRelayPacket::Query(QueryP::PubAddress).send(&mut tx).await?;

        tx.finish()?;

        match unpack(&mut rx).await.map_err(|e| anyhow!("failed to unpack packet: {e}"))? {
            SRelayPacket::QueryResult(QueryResultP::PubAddress { addr }) => Ok(addr),
            unknown => Err(anyhow!("got unknown response: {unknown:?}")),
        }
    }
}

fn handle_deliver(ipk: VerifyingKey, msg: DeliverP) {
    // 1. Check if sender is a known contact
    let Some(contact) = Contact::get(&msg.from) else {
        info!("MESSAGE: dropped message from unknown sender {}", hex::encode(msg.from));
        return;
    };

    // 2. Derive per-friendship shared key and decrypt
    let Ok(shared_key) =
        contact.shared_key().inspect_err(|e| warn!("MESSAGE: failed to derive shared key: {e}"))
    else {
        return;
    };

    let Some(encrypted) = decode_encrypted(&msg.payload) else {
        warn!("MESSAGE: payload too short from {}", hex::encode(msg.from));
        return;
    };

    let Ok(plaintext) = encrypted.decrypt(&shared_key, ipk.as_bytes()) else {
        warn!("MESSAGE: decryption failed from {}", hex::encode(msg.from));
        return;
    };

    let Ok(content) = String::from_utf8(plaintext) else {
        warn!("MESSAGE: invalid UTF-8 from {}", hex::encode(msg.from));
        return;
    };

    let timestamp = systime().as_secs();

    info!("MESSAGE: received from {}", hex::encode(msg.from));

    // 3. Save to local DB
    match Message::save_incoming(*msg.from, &content, timestamp) {
        Ok(m) => {
            MessageEv::Received { id: m.inner.id, from: *msg.from, content, timestamp }.emit();
        },
        Err(e) => {
            warn!("MESSAGE: failed to save incoming: {e}");
        },
    };
}
