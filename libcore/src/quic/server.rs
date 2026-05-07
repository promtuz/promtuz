use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
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
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::proto::pack::Unpacker;
use common::proto::pack::unpack;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use ed25519_dalek::VerifyingKey;
use log::debug;
use log::error;
use log::info;
use log::warn;
use parking_lot::RwLock;
use quinn::ConnectionError;
use quinn::SendStream;
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
use crate::ret_err;
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

// const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: usize = 16;

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

    /// Build and send a one-shot `CRelayPacket::DrainAuth` permit so this relay
    /// can pull our offline-queue from the K-closest DHT homes on our behalf.
    ///
    /// The transcript binds (self_ipk, this_relay_id, timestamp); the same
    /// signature is reusable across all K homes (no per-home identity in the
    /// transcript) within the ±60s skew window. Phase 2c sticky-home flow.
    async fn send_drain_auth(
        &self, conn: &quinn::Connection, ipk: VerifyingKey,
    ) -> Result<()> {
        let timestamp = systime().as_millis() as u64;
        let relay_node_id = NodeId::from_str(&self.id)
            .map_err(|e| anyhow!("relay id {:?} not parseable as NodeId: {e:?}", self.id))?;
        let self_ipk = ipk.to_bytes();
        let transcript = queue_fetch_signing_input(&self_ipk, &relay_node_id, timestamp);
        let sig = IdentitySigner::sign(&transcript)?;

        let (mut tx, _rx) = conn.open_bi().await?;
        let packet = CRelayPacket::DrainAuth {
            timestamp,
            sig: Bytes::from(sig.to_bytes()),
        };
        packet.send(&mut tx).await?;
        _ = tx.finish();
        Ok(())
    }

    // TODO: make custom error type for relay handling and handle it, supporting io errors from
    // send, unpack etc utils
    fn handle_err(&self, err: &ConnectionError) {
        ConnectionState::Disconnected.emit();
        _ = self.record_failure();

        // Only clear RELAY if it still points to this relay.
        // A reconnect may have already replaced it.
        // FIXME: it might've reconnected to itself so checking only id is not good
        let mut guard = RELAY.write();
        if guard.as_ref().map(|r| r.id == self.id).unwrap_or(false) {
            *guard = None;
        }

        error!("relay({}) connection lost: {err}", self.id);
    }

    /// Waits for incoming streams. Runs until the connection is lost.
    async fn handle(&self, ipk: VerifyingKey) -> ConnectionError {
        let conn = self.connection.as_ref().expect("handle called without active connection");
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS));

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        // Sticky-home auth: hand the relay a one-shot signed permit it can use to
        // QueueFetch our offline queue from the K-closest homes. Sig is reusable
        // across all K homes within the ±60s skew window. Best-effort; if it
        // fails we still proceed with DrainQueue (relay will only be able to
        // serve its own local queue, falling back to natural TTL convergence).
        if let Err(err) = self.send_drain_auth(conn, ipk).await {
            warn!("relay({}) drain-auth send failed: {err}", self.id);
        }

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        // Draining Queue

        {
            let (mut tx, mut rx) =
                ret_err!(conn.open_bi().await.inspect_err(|e| self.handle_err(e)));

            if CRelayPacket::DrainQueue.send(&mut tx).await.is_err() {
                return ConnectionError::LocallyClosed;
            }

            // let Ok(SRelayPacket::QueueDrain(messages)) = SRelayPacket::unpack(&mut rx).await else
            // {     return ConnectionError::LocallyClosed;
            // };

            _ = tx.finish();
        }

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        let relay_id = self.id.clone();

        debug!("waiting for incoming streams from relay({})", relay_id);

        loop {
            let (mut send, mut recv) = ret_err!(conn.accept_bi().await);

            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    debug!("relay({}) stream limit reached, dropping stream", relay_id);
                    continue;
                },
            };

            let relay_id = relay_id.clone();
            tokio::spawn(async move {
                let _permit = permit; // dropped when stream task ends
                while let Ok(packet) = SRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = match packet {
                        SRelayPacket::Deliver(msg) => handle_deliver(&mut send, ipk, msg).await,
                        other => {
                            debug!("unexpected packet from relay: {other:?}");
                            Ok(())
                        },
                    } {
                        warn!("relay({}) handle err: {err}", relay_id);
                    }
                }
            });
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

async fn handle_deliver(tx: &mut SendStream, ipk: VerifyingKey, msg: DeliverP) -> Result<()> {
    // 1. Check if sender is a known contact
    let Some(contact) = Contact::get(&msg.from) else {
        info!("MESSAGE: dropped message from unknown sender {}", hex::encode(msg.from));
        bail!("unknown sender");
    };

    // 2. Derive per-friendship shared key and decrypt
    let Ok(shared_key) =
        contact.shared_key().inspect_err(|e| warn!("MESSAGE: failed to derive shared key: {e}"))
    else {
        bail!("failed to derive shared key");
    };

    let Some(encrypted) = decode_encrypted(&msg.payload) else {
        warn!("MESSAGE: payload too short from {}", hex::encode(msg.from));
        bail!("payload too short")
    };

    let Ok(plaintext) = encrypted.decrypt(&shared_key, ipk.as_bytes()) else {
        warn!("MESSAGE: decryption failed from {}", hex::encode(msg.from));
        bail!("decryption failed")
    };

    let Ok(content) = String::from_utf8(plaintext) else {
        warn!("MESSAGE: invalid UTF-8 from {}", hex::encode(msg.from));
        bail!("invalid UTF-8")
    };

    let timestamp = systime().as_secs();

    // Persist BEFORE acking. If we ack first and the relay dequeues, a crash
    // (or DB failure) between ack and save loses the message permanently.
    let saved = match Message::save_incoming(*msg.from, &content, timestamp) {
        Ok(m) => m,
        Err(e) => {
            warn!("MESSAGE: failed to save incoming: {e}");
            // Skip the ack so the relay redelivers next time.
            bail!("save failed: {e}");
        },
    };

    CRelayPacket::DeliverAck.send(tx).await?;

    info!("MESSAGE: received from {}", hex::encode(msg.from));
    MessageEv::Received { id: saved.inner.id, from: *msg.from, content, timestamp }.emit();

    Ok(())
}
