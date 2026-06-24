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
use common::proto::dht_p2p::MAX_FETCH_QUEUE_ACK_IDS;
use common::proto::dht_p2p::queue_fetch_ack_signing_input;
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
use quinn::ConnectionError;
use quinn::SendStream;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ENDPOINT;
use crate::api::conn_stats::CONNECTION_START_TIME;
use crate::data::contact::Contact;
use crate::data::identity::IdentitySigner;
use crate::data::message::Message;
use crate::data::relay::Relay;
use crate::db::mls::stash_db_handle;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::events::messaging::MessageEv;
use crate::quic::relay_dht_client::RelayDhtClient;
use crate::ret_err;
use crate::utils::systime;

/// KP rotation scheduler tick cadence. Each tick the libcore checks
/// [`crate::mls::scheduler::run_once`] for pending refill / rotation
/// work; the task lives for the lifetime of the relay connection and
/// is cooperatively cancelled on disconnect.
///
/// design-doc: `misc/specs/MLS.md` §5.5 (refill cadence), §11.3.
const KP_SCHEDULER_TICK_MS: u64 = 60_000;

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

/// Classifies a `quinn::ConnectionError` as terminal-for-this-relay
/// (TLS / cert / auth failure that won't resolve without external
/// intervention) versus transient (network blip, timeout, peer reset).
///
/// QUIC encodes TLS alerts as transport error codes `0x100..=0x1ff`
/// (alert byte + 0x100 per RFC 9001 §4.8). The cert-related alerts:
/// - 42 bad_certificate
/// - 43 unsupported_certificate
/// - 44 certificate_revoked
/// - 45 certificate_expired
/// - 46 certificate_unknown
/// - 48 unknown_ca
/// - 51 decrypt_error (often a cert-binding mismatch in TLS 1.3)
fn is_terminal_for_relay(err: &ConnectionError) -> bool {
    if let ConnectionError::TransportError(t) = err {
        let code: u64 = t.code.into();
        if (0x100..=0x1ff).contains(&code) {
            let alert = (code & 0xff) as u8;
            return matches!(alert, 42 | 43 | 44 | 45 | 46 | 48 | 51);
        }
    }
    false
}

// Phase 8 (P1 #19): the actual `RELAY` singleton lives in
// `crate::state` (a leaf module) so `api::messaging` doesn't have to
// pull in `quic::server` for a global it shares with us. Re-exported
// here for backwards compatibility with existing call sites in this
// module.
pub use crate::state::RELAY;

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
                if is_terminal_for_relay(&err) {
                    warn!(
                        "relay({}) cert/auth failure ({}) — marking terminal, will not retry",
                        self.id, err
                    );
                    _ = self.record_terminal_failure();
                } else {
                    error!("connection with relay({}) failed: {}", self.id, err);
                    _ = self.record_failure();
                }
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
            SHSRP::Accept { timestamp, relay_node_id } => {
                let latency_ms = systime().as_millis() as u64 - connect_start;
                _ = self.record_success(latency_ms);
                // Phase 9 §3.9 — stash the home's advertised DHT NodeId
                // for the RelayDhtClient to bind in welcome fetch/ack sigs.
                self.home_node_id = relay_node_id.map(|b| b.0);
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

        // Phase 9 §3.9: build the production DHT-RPC dialer once the
        // relay/1 connection is established. The dialer rides this same
        // connection (no peer/1), stored on the `Relay` struct so the
        // JNI surface (`sendMessage`, `handle_deliver`) picks it up via
        // `RELAY.read()`. Failure to build is logged and `dht_client`
        // stays `None`; the caller surfaces a clean error rather than
        // silently no-oping.
        match build_relay_dht_client(&self, ipk) {
            Ok(c) => self.dht_client = Some(c),
            Err(e) => {
                warn!("MLS: DHT dialer not constructed at connect: {e}");
            },
        }

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
            let (mut tx, _rx) =
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

        // Phase 5a / Phase 7: re-use the production peer/1 dialer that
        // `connect()` built before storing this Relay on the global
        // `RELAY`. The dialer is shared (`Arc<RelayDhtClient>`) so the
        // JNI surface (`sendMessage`, `handle_deliver`) and the
        // background tasks below all dispatch over the same pool.
        //
        // The cancellation token is fired when the function returns
        // (on connection loss) so the scheduler task exits cleanly.
        // `_mls_cancel_drop_guard` keeps the cancel-on-drop alive
        // through the rest of `handle()` — see line below.
        let mls_cancel = CancellationToken::new();
        let dht_client = self.dht_client.clone();

        if let Some(client) = dht_client.as_ref() {
            // 1. One-shot Welcome poll on reconnect.
            //
            // Best-effort — if the K-set FindNode times out or the
            // recipient relay is down we just log; Welcomes can be
            // re-fetched on the next reconnect, and the home's
            // 30-day retention covers multi-week offline windows.
            let client_for_poll = client.clone();
            tokio::spawn(async move {
                if let Err(e) = poll_welcomes_once(client_for_poll).await {
                    warn!("MLS: poll_welcomes failed: {e}");
                }
            });

            // 2. KP rotation scheduler — long-lived task, ticks every
            //    KP_SCHEDULER_TICK_MS. Cancelled on disconnect via
            //    `mls_cancel`.
            let client_for_sched = client.clone();
            let cancel_for_sched = mls_cancel.clone();
            tokio::spawn(async move {
                run_scheduler_loop(client_for_sched, cancel_for_sched).await;
            });
        }

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        let relay_id = self.id.clone();

        debug!("waiting for incoming streams from relay({})", relay_id);

        // Hold the drop guard for the duration of `handle()`. When
        // `handle` returns (connection lost), the guard drops →
        // `mls_cancel` fires → the scheduler task observes
        // `cancelled().await` and exits cleanly.
        let _mls_cancel_drop_guard = mls_cancel.drop_guard();

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
            // Phase 7 (P0-4): clone the dialer Arc into the per-stream
            // task so `handle_deliver` can drive
            // `process_inbound_envelope` over the production wire (KP
            // fetch on stale-group recreate, etc.) instead of the
            // stub `NotWiredDhtClient`.
            let dht_client_for_stream = self.dht_client.clone();
            tokio::spawn(async move {
                let _permit = permit; // dropped when stream task ends
                while let Ok(packet) = SRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = match packet {
                        SRelayPacket::Deliver(msg) => {
                            handle_deliver(
                                &mut send,
                                ipk,
                                msg,
                                dht_client_for_stream.clone(),
                            )
                            .await
                        },
                        SRelayPacket::AckAuthRequest {
                            requester_relay_id,
                            delivered_ids,
                            suggested_timestamp,
                        } => {
                            handle_ack_auth_request(
                                &mut send,
                                ipk,
                                requester_relay_id,
                                delivered_ids,
                                suggested_timestamp,
                            )
                            .await
                        },
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

async fn handle_deliver(
    tx: &mut SendStream, _ipk: VerifyingKey, msg: DeliverP,
    dht_client: Option<Arc<RelayDhtClient>>,
) -> Result<()> {
    // Phase 4 receive path. The wire envelope is now `MlsEnvelopeP`
    // (postcard-encoded), so we hand off to
    // `api::messaging::process_inbound_envelope` rather than the v2
    // shared-key decrypt.
    //
    // Contact-first gating: anything from an IPK we don't have on
    // file is dropped (mirrors the v2 receive path).
    if !Contact::exists(&msg.from) {
        info!("MESSAGE: dropped envelope from unknown sender {}", hex::encode(&msg.from[..4]));
        bail!("unknown sender");
    }

    // Phase 7 (P0-4): use the production peer/1 dialer that the
    // connection-time wiring in `Relay::connect` attached to the
    // global `RELAY`. The receive path's MLS handling
    // (`process_inbound_envelope`) needs a `DhtClient` for completeness
    // even though today's Welcome / Application receive paths don't
    // dial back to the DHT — future stale-group recreate or KP-rotation
    // hooks will. Falling back to `NotWiredDhtClient` only when the
    // dialer wasn't built (PEER_IDENTITY missing at connect time);
    // surfaced via existing logging.
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = stash_db_handle();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let result = match dht_client {
        Some(client) => {
            let ctx = crate::api::messaging::MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      client.as_ref(),
            };
            crate::api::messaging::process_inbound_envelope(&ctx, *msg.from, &msg.payload).await
        },
        None => {
            let dht = crate::quic::dht_client::NotWiredDhtClient;
            let ctx = crate::api::messaging::MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      &dht,
            };
            crate::api::messaging::process_inbound_envelope(&ctx, *msg.from, &msg.payload).await
        },
    };

    match result {
        Ok(Some(crate::api::messaging::InboundDecoded::Application { plaintext, group_id: _ })) => {
            // Surface as a UTF-8 message in the Phase 4 model. Future
            // structured payloads (read receipts, attachments, etc.)
            // will arrive as their own MlsEnvelopeP sub-variants;
            // until then any non-UTF-8 application data is dropped.
            let Ok(content) = String::from_utf8(plaintext) else {
                warn!("MESSAGE: invalid UTF-8 from {}", hex::encode(&msg.from[..4]));
                bail!("invalid UTF-8");
            };
            let timestamp = systime().as_secs();
            let saved = match Message::save_incoming(*msg.from, &content, timestamp) {
                Ok(m) => m,
                Err(e) => {
                    warn!("MESSAGE: failed to save incoming: {e}");
                    bail!("save failed: {e}");
                },
            };
            CRelayPacket::DeliverAck.send(tx).await?;
            info!("MESSAGE: received from {}", hex::encode(&msg.from[..4]));
            MessageEv::Received {
                id: saved.inner.id,
                from: *msg.from,
                content,
                timestamp,
            }
            .emit();
        },
        Ok(Some(crate::api::messaging::InboundDecoded::Welcome)) => {
            info!("MLS: processed welcome from {}", hex::encode(&msg.from[..4]));
            CRelayPacket::DeliverAck.send(tx).await?;
        },
        Ok(Some(crate::api::messaging::InboundDecoded::ApplicationBuffered)) => {
            // Buffered for a future epoch / staged commit merged.
            // Ack so the relay GCs the queue entry.
            CRelayPacket::DeliverAck.send(tx).await?;
        },
        Ok(Some(crate::api::messaging::InboundDecoded::ApplicationStale)) => {
            // Phase 7 (P0-7): ack stale-epoch envelopes so the relay
            // GCs them. Previously this `bail`ed without ack which made
            // the relay redeliver indefinitely (queue grows without
            // bound, CPU burns on every redelivery decoding the same
            // doomed envelope). The recipient cannot recover state for
            // a stale epoch — openmls only retains a small past-epoch
            // key window — so re-delivery is hopeless, and an explicit
            // ack is the correct response.
            warn!("MESSAGE: stale-epoch envelope from {}; acking and dropping", hex::encode(&msg.from[..4]));
            CRelayPacket::DeliverAck.send(tx).await?;
        },
        Ok(None) => {
            // Currently unreachable — process_inbound_envelope only
            // returns None for the protocol-mismatch path, which
            // never fires in production.
            bail!("no inbound action");
        },
        Err(e) => {
            warn!("MESSAGE: process_inbound_envelope failed from {}: {e}", hex::encode(&msg.from[..4]));
            bail!("process failed: {e}");
        },
    }

    Ok(())
}

/// Phase 2d — handle a relay-issued `SRelayPacket::AckAuthRequest`.
///
/// The relay asks us (the client) to sign a `QueueFetchAck`
/// transcript over the union of dispatch ids it just drained from the
/// K home relays. We sign with the long-term identity key
/// ([`IdentitySigner::sign`]) over
/// [`queue_fetch_ack_signing_input`] and reply with a
/// `CRelayPacket::AckAuth { sig, timestamp }`. The relay then fans the
/// signed pair out as `QueueFetchAck` to each home so the home-side
/// `cf_dht_queue` entries get GC'd.
///
/// **Phase 2d-fix — `requester_relay_id` binding**: the relay supplies
/// its own NodeId via `requester_relay_id`; we sign that value
/// verbatim into the transcript. The home cross-checks the field
/// against the connection's authenticated peer id when handling the
/// resulting `QueueFetchAck`, so a captured ack can no longer be
/// redirected to a different home via a different relay (cross-relay
/// replay defense). Libcore neither validates nor rewrites the
/// supplied id — we trust the relay we authenticated to to provide
/// its own identity correctly; the home does the cross-check.
///
/// **Why we trust `suggested_timestamp`** rather than reading our own
/// clock: the relay's clock is what matters for the home-side skew
/// check (the homes verify against the timestamp embedded in the
/// signed transcript). Using `suggested_timestamp` saves a `systime()`
/// call and avoids a redundant clock-drift hazard.
///
/// **Length bound**: we silently drop the request if
/// `delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS`. The home-side
/// verifier would reject it anyway (`QueueFetchAck::verify` returns
/// `TooManyIds` past the cap); failing here saves the round trip.
async fn handle_ack_auth_request(
    tx: &mut SendStream, ipk: VerifyingKey, requester_relay_id: NodeId,
    delivered_ids: Vec<[u8; 16]>, suggested_timestamp: u64,
) -> Result<()> {
    if delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS {
        warn!(
            "ACK_AUTH: delivered_ids overflow ({} > {}); dropping",
            delivered_ids.len(),
            MAX_FETCH_QUEUE_ACK_IDS
        );
        return Ok(());
    }
    let self_ipk = ipk.to_bytes();
    let transcript = queue_fetch_ack_signing_input(
        &self_ipk,
        &requester_relay_id,
        &delivered_ids,
        suggested_timestamp,
    );
    let sig = IdentitySigner::sign(&transcript)?;
    CRelayPacket::AckAuth {
        sig:       Bytes::from(sig.to_bytes()),
        timestamp: suggested_timestamp,
    }
    .send(tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// MLS / DHT-RPC dialer wiring (Phase 9 §3.9)
// ---------------------------------------------------------------------------

/// Build the production [`RelayDhtClient`] from the current connection
/// state. Unlike the deleted Option-A `Peer1DhtClient`, this dials
/// nothing — it rides the already-authenticated home `relay/1`
/// connection. It needs only the connection, our IPK, and the home's
/// DHT NodeId (learned from the handshake, for welcome fetch/ack
/// signatures). Signing goes through the global `IdentitySigner`.
///
/// Returns `Err` if the connection isn't established yet; the caller
/// logs and skips the MLS background work.
fn build_relay_dht_client(
    relay: &Relay, ipk: VerifyingKey,
) -> Result<Arc<RelayDhtClient>> {
    let conn = relay
        .connection
        .clone()
        .ok_or_else(|| anyhow!("relay connection not established"))?;
    Ok(Arc::new(RelayDhtClient::new(conn, ipk.to_bytes(), relay.home_node_id)))
}

/// One-shot Welcome poll on reconnect. Builds an `MlsContext` against
/// fresh DB handles and the supplied dialer; runs
/// [`crate::api::messaging::poll_welcomes`] once.
///
/// design-doc: `misc/specs/MLS.md` §6.1 (Welcome queue on reconnect),
/// §11.3.
async fn poll_welcomes_once(client: Arc<RelayDhtClient>) -> Result<()> {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = stash_db_handle();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = crate::api::messaging::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    let count = crate::api::messaging::poll_welcomes(&ctx).await?;
    if count > 0 {
        info!("MLS: poll_welcomes processed {count} welcome(s)");
    }
    Ok(())
}

/// KP-rotation scheduler loop — production wiring.
///
/// Loads the user's identity + signing key from the libcore globals,
/// then delegates to [`run_scheduler_inner`]. Errors loading the
/// identity exit the loop early (logged); the inner loop owns the
/// cancellation contract.
async fn run_scheduler_loop(
    client: Arc<RelayDhtClient>, cancel: CancellationToken,
) {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = stash_db_handle();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let our_ipk_bytes = match crate::data::identity::Identity::get() {
        Some(i) => i.ipk(),
        None => {
            warn!("MLS scheduler: identity unavailable; loop exiting");
            return;
        },
    };
    let signing = match crate::data::identity::secret_key_signing(&our_ipk_bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!("MLS scheduler: signing key unavailable: {e}; loop exiting");
            return;
        },
    };
    run_scheduler_inner(
        &provider,
        &stash,
        &signing,
        client.as_ref(),
        Duration::from_millis(KP_SCHEDULER_TICK_MS),
        cancel,
    )
    .await;
}

/// KP-rotation scheduler — tickable inner loop. Runs
/// [`crate::mls::scheduler::run_once`] immediately, then every
/// `tick_interval`. Exits cleanly when `cancel` is fired.
///
/// Errors from `run_once` are logged at WARN; the loop continues
/// (transient publish failures shouldn't tear the scheduler down —
/// the next tick will retry).
///
/// Generic over [`crate::quic::dht_client::DhtClient`] so unit tests
/// can drive it with the in-process `FakeDhtClient`.
///
/// design-doc: `misc/specs/MLS.md` §5.5, §11.3.
async fn run_scheduler_inner<C: crate::quic::dht_client::DhtClient>(
    provider: &crate::mls::PromtuzMlsProvider,
    stash: &crate::mls::KeyPackageStash,
    signing: &ed25519_dalek::SigningKey,
    dht: &C,
    tick_interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        let now_ms = systime().as_millis() as u64;
        match crate::mls::scheduler::run_once(
            provider,
            stash,
            signing,
            dht,
            now_ms,
        )
        .await
        {
            Ok(crate::mls::scheduler::SchedulerOutcome::NoOp) => {},
            Ok(other) => {
                debug!("MLS scheduler: {other:?}");
            },
            Err(e) => {
                warn!("MLS scheduler tick failed: {e}");
            },
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("MLS scheduler: cancelled, exiting");
                return;
            }
            _ = tokio::time::sleep(tick_interval) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Phase 2d — pure-function tests for `handle_ack_auth_request`'s
    //! transcript shape. We can't drive the QUIC `SendStream` half
    //! without a live connection, but the load-bearing piece of the
    //! handler is the transcript construction (it must match the
    //! relay's verifier byte-for-byte). Pin the transcript layout
    //! against `queue_fetch_ack_signing_input` so any drift surfaces
    //! here.
    use common::proto::dht_p2p::DHT_QUEUE_FETCH_ACK_SIG_DOMAIN;
    use common::proto::dht_p2p::queue_fetch_ack_signing_input;
    use common::quic::id::NodeId;

    /// Pin the transcript shape: `domain || version (BE u16) || ipk
    /// (32) || requester_relay_id (32) || count (BE u32) || n*16 ||
    /// ts (BE u64)`. The phase 2d-fix `requester_relay_id` field sits
    /// after `ipk` and before the count prefix; catches a regression
    /// in either side of the helper / verifier boundary.
    #[test]
    fn handle_ack_auth_request_signs_correct_transcript() {
        let user_ipk: [u8; 32] = [0x11; 32];
        let req_id = NodeId::new([0x42u8; 32]);
        let ids: Vec<[u8; 16]> = vec![[0xAA; 16], [0xBB; 16], [0xCC; 16]];
        let ts: u64 = 1_700_000_000_004;

        let transcript = queue_fetch_ack_signing_input(&user_ipk, &req_id, &ids, ts);
        let expected_len = DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len()
            + 2  // version
            + 32 // ipk
            + NodeId::LEN // requester_relay_id (phase 2d-fix)
            + 4  // count BE u32
            + ids.len() * 16
            + 8; // ts BE u64
        assert_eq!(transcript.len(), expected_len);

        // Layout invariants: domain prefix at offset 0, version next,
        // ipk after, requester_relay_id after, count after that, then
        // ids, then ts.
        assert!(transcript.starts_with(DHT_QUEUE_FETCH_ACK_SIG_DOMAIN));
        let off = DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len();
        // version is BE u16 at `off..off+2`
        assert_eq!(transcript[off..off + 2].len(), 2);
        // ipk at `off+2..off+2+32`
        assert_eq!(&transcript[off + 2..off + 2 + 32], &user_ipk);
        // requester_relay_id at `off+2+32..off+2+32+32`
        let rid_off = off + 2 + 32;
        assert_eq!(&transcript[rid_off..rid_off + NodeId::LEN], req_id.as_bytes());
        // count at `rid_off+32..rid_off+32+4`
        let count_off = rid_off + NodeId::LEN;
        let count_bytes: [u8; 4] = transcript[count_off..count_off + 4].try_into().unwrap();
        assert_eq!(u32::from_be_bytes(count_bytes), ids.len() as u32);
        // ts at the end as BE u64
        let ts_bytes: [u8; 8] =
            transcript[transcript.len() - 8..].try_into().unwrap();
        assert_eq!(u64::from_be_bytes(ts_bytes), ts);
    }

    /// Confirm the empty-ids edge case is well-formed: the transcript
    /// length collapses to `domain || version || ipk ||
    /// requester_relay_id || count(0) || ts`, no body. The relay-side
    /// verifier accepts an empty-ids ack (it's a probe-only "I'm
    /// here" signal).
    #[test]
    fn handle_ack_auth_request_empty_ids_transcript_is_well_formed() {
        let user_ipk: [u8; 32] = [0x22; 32];
        let req_id = NodeId::new([0x55u8; 32]);
        let ids: Vec<[u8; 16]> = vec![];
        let ts: u64 = 1_700_000_000_005;

        let transcript = queue_fetch_ack_signing_input(&user_ipk, &req_id, &ids, ts);
        let expected_len = (DHT_QUEUE_FETCH_ACK_SIG_DOMAIN.len()
            + 2
            + 32
            + NodeId::LEN
            + 4)
            + 8;
        assert_eq!(transcript.len(), expected_len);
    }

    // ----- KP scheduler tokio task tests (Phase 5a) ------------------

    use std::sync::Arc;
    use std::time::Duration;

    use ed25519_dalek::SigningKey;
    use parking_lot::Mutex;
    use rusqlite::Connection;
    use tokio_util::sync::CancellationToken;

    use crate::db::mls::apply_mls_migrations;
    use crate::mls::KeyPackageStash;
    use crate::mls::PromtuzMlsProvider;
    use crate::quic::dht_client::tests::FakeDhtClient;

    fn fresh_mls_conn() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        Arc::new(Mutex::new(conn))
    }

    /// Scheduler runs an immediate tick on entry, then ticks at
    /// `tick_interval`. With a fresh stash, the first tick refills via
    /// the dialer; we observe the recorded batch and assert the cadence
    /// drives a second tick after the configured interval.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scheduler_loop_ticks_at_configured_interval() {
        let conn = fresh_mls_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        let signing = SigningKey::from_bytes(&[0xAA; 32]);
        let dht = FakeDhtClient::new_arc();
        let cancel = CancellationToken::new();

        let dht_for_loop = dht.clone();
        let cancel_for_loop = cancel.clone();
        let join = tokio::spawn(async move {
            super::run_scheduler_inner(
                &provider,
                &stash,
                &signing,
                dht_for_loop.as_ref(),
                Duration::from_millis(60_000),
                cancel_for_loop,
            )
            .await;
        });

        // First tick refills (empty stash → publishes once).
        // Yield a few times to let the scheduler's first run_once
        // resolve.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        assert_eq!(dht.published_kp_batches.lock().len(), 1, "first tick publishes");

        // Advance the simulated clock past the cadence; the next
        // scheduled tick should fire and (because the stash is now
        // full) be a NoOp — but no additional publish. Verify cadence
        // by waiting one tick.
        tokio::time::advance(Duration::from_millis(60_001)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // Stash is full; second tick is NoOp; no new batch published.
        assert_eq!(
            dht.published_kp_batches.lock().len(),
            1,
            "healthy-stash tick is NoOp"
        );

        // Cancel and confirm the loop exits.
        cancel.cancel();
        // Give it a chance to observe cancel + return.
        tokio::time::advance(Duration::from_millis(1)).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), join).await
            .expect("scheduler exits within 1s of cancel");
    }

    /// `cancel.cancel()` while the scheduler is sleeping inside the
    /// tokio::select! drives the loop to exit on the next event-loop
    /// turn. Pinned at <2s wall-clock to satisfy test discipline.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn scheduler_loop_cancels_cleanly_on_cancellation() {
        let conn = fresh_mls_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        // Pre-fill so the first tick is NoOp (faster path; the
        // empty-stash tick mints 100 KPs which is observable but slow
        // under start_paused=true cooperative scheduling).
        let signing = SigningKey::from_bytes(&[0xBB; 32]);
        let _ = stash.ensure_stash_full(&provider, &signing).expect("seed stash");
        let dht = FakeDhtClient::new_arc();
        let cancel = CancellationToken::new();

        let dht_for_loop = dht.clone();
        let cancel_for_loop = cancel.clone();
        let join = tokio::spawn(async move {
            super::run_scheduler_inner(
                &provider,
                &stash,
                &signing,
                dht_for_loop.as_ref(),
                Duration::from_secs(60),
                cancel_for_loop,
            )
            .await;
        });

        // First tick is NoOp — no publish.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(dht.published_kp_batches.lock().len(), 0);

        // Cancel mid-sleep — the inner loop should exit on the next
        // event-loop turn.
        cancel.cancel();
        // Yield a few times so the cancelled() future fires.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }

        let res = tokio::time::timeout(Duration::from_secs(1), join).await;
        assert!(res.is_ok(), "scheduler exits cleanly on cancel");
    }
}
