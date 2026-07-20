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
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::ReceiptKind;
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
use crate::state::CONNECTION_START_TIME;
use crate::utils::systime;

/// KP rotation scheduler tick cadence. Each tick the libcore checks
/// [`crate::mls::scheduler::run_once`] for pending refill / rotation
/// work; the task lives for the lifetime of the relay connection and
/// is cooperatively cancelled on disconnect.
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

/// Bound the QUIC connect so an unreachable relay fails fast and the loop
/// rolls to the next one, instead of hanging on quinn's default idle timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: usize = 16;
/// Cadence for sampling the live connection RTT into the latency graph.
const RTT_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

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

// The actual `RELAY` singleton lives in `crate::state` (a leaf module)
// so `api::messaging` doesn't have to pull in `quic::server` for a
// global it shares with us. Re-exported here for backwards
// compatibility with existing call sites in this module.
pub use crate::state::RELAY;

impl Relay {
    pub async fn connect(
        mut self, ipk: VerifyingKey,
    ) -> Result<JoinHandle<ConnectionError>, RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host)?, self.port);

        info!("connecting to relay({}) at {}", self.id, addr);
        ConnectionState::Connecting.emit();

        let connect_start = systime().as_millis() as u64;

        let connecting = ENDPOINT.get().unwrap().connect(addr, &self.id)?;
        let conn = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(err)) => {
                ConnectionState::Failed.emit();
                if is_terminal_for_relay(&err) {
                    warn!(
                        "relay({}) at {addr} cert/auth failure ({err}) — terminal, will not retry",
                        self.id
                    );
                    _ = self.record_terminal_failure();
                } else {
                    error!("relay({}) at {addr} connect failed: {err}", self.id);
                    _ = self.record_failure();
                }
                return Err(RelayConnError::Continue);
            },
            Err(_) => {
                warn!(
                    "relay({}) at {addr} unreachable — timed out after {}s",
                    self.id,
                    CONNECT_TIMEOUT.as_secs()
                );
                ConnectionState::Failed.emit();
                _ = self.record_failure();
                return Err(RelayConnError::Continue);
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

        let timestamp = match result {
            SHSRP::Accept { timestamp, relay_node_id } => {
                // Stash the home's advertised DHT NodeId for the
                // RelayDhtClient to bind in welcome fetch/ack sigs.
                self.home_node_id = relay_node_id.map(|b| b.0);
                timestamp
            },
            SHSRP::Reject { reason } => {
                warn!("relay handshake failed : {reason}");
                _ = self.record_failure();
                return Err(RelayConnError::Continue);
            },
        };

        let connect_ms = systime().as_millis() as u64 - connect_start;
        info!("authenticated with relay({}) in {connect_ms}ms at {timestamp}", self.id);
        CONNECTION_START_TIME.store(timestamp, Ordering::Relaxed);
        // Auth is up but the offline backlog (welcomes, deferred sends, queued
        // messages) isn't drained yet — surface that as "Syncing…". `handle`
        // flips to Connected once it's pulled; failures below emit Disconnected,
        // so we never stick here.
        ConnectionState::Syncing.emit();

        self.record_success().map_err(|e| RelayConnError::Error(e.into()))?;

        // Live RTT sampler — quinn's smoothed round-trip estimate, sampled
        // while the connection lives. This is the "ping" the relays page
        // graphs and the latency term `fetch_best` scores on; it ends itself
        // when the connection closes.
        tokio::spawn({
            let relay = self.clone();
            let conn = conn.clone();
            async move { relay.sample_rtt(&conn).await }
        });

        self.connection = Some(conn.clone());

        // Build the production DHT-RPC dialer once the relay/1 connection
        // is established. The dialer rides this same connection, stored on
        // the `Relay` struct so the JNI surface (`sendMessage`,
        // `handle_deliver`) picks it up via `RELAY.read()`. Failure to
        // build is logged and `dht_client` stays `None`; the caller
        // surfaces a clean error rather than silently no-oping.
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

        // Lease is short-lived by design. Refresh below half-life while this
        // exact home connection remains live; reconnect also re-registers it.
        tokio::spawn({
            let conn = conn.clone();
            async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(4 * 60));
                tick.tick().await;
                while conn.close_reason().is_none() {
                    tick.tick().await;
                    if let Err(e) = crate::messaging::renew_presence_lease().await {
                        debug!("presence lease renewal failed: {e}");
                    }
                }
            }
        });

        // Re-assert real fg/bg presence FIRST: a headless push wake-drain
        // reconnects with no UI alive. The relay now defaults a reconnect to
        // Offline (connection alone is not presence), so this is what re-asserts
        // Active on a live-FOREGROUND reconnect. Ahead of the push registrations
        // (which await network RPCs and can stall) so a UI re-subscribe can't
        // beat it to the wire. Runs here (after the RELAY.write above) so
        // set_presence sees the live connection.
        //
        // Then register our push-pseudonym so this home can wake us when
        // offline, and (re)register P→token with a gateway if we hold a token.
        tokio::spawn(async {
            if let Err(e) = crate::messaging::reassert_presence().await {
                debug!("PRESENCE: reassert on connect failed: {e}");
            }
            if let Err(e) = crate::push::register_push().await {
                warn!("register_push failed: {e}");
            }
            if let Err(e) = crate::push::register_token_at_gateway().await {
                debug!("register_token_at_gateway failed: {e}");
            }
        });

        Ok(handle)
    }

    /// Samples `conn.rtt()` (quinn's smoothed round-trip estimate) every
    /// [`RTT_SAMPLE_INTERVAL`] and records it, until the connection closes.
    /// Runs as a detached task spawned at connect; `close_reason()` turning
    /// `Some` is the exit signal, so it needs no external cancellation.
    async fn sample_rtt(&self, conn: &quinn::Connection) {
        while conn.close_reason().is_none() {
            let rtt_ms = conn.rtt().as_millis() as u64;
            if let Err(e) = self.record_rtt(rtt_ms) {
                warn!("relay({}) rtt sample failed: {e}", self.id);
            }
            tokio::time::sleep(RTT_SAMPLE_INTERVAL).await;
        }
    }

    /// Build and send a one-shot `CRelayPacket::DrainAuth` permit so this relay
    /// can pull our offline-queue from the K-closest DHT homes on our behalf.
    ///
    /// The transcript binds (self_ipk, this_relay_id, timestamp); the same
    /// signature is reusable across all K homes (no per-home identity in the
    /// transcript) within the ±60s skew window. Part of the sticky-home flow.
    async fn send_drain_auth(&self, conn: &quinn::Connection, ipk: VerifyingKey) -> Result<()> {
        let timestamp = systime().as_millis() as u64;
        let relay_node_id = NodeId::from_str(&self.id)
            .map_err(|e| anyhow!("relay id {:?} not parseable as NodeId: {e:?}", self.id))?;
        let self_ipk = ipk.to_bytes();
        let transcript = queue_fetch_signing_input(&self_ipk, &relay_node_id, timestamp);
        let sig = IdentitySigner::sign(&transcript)?;

        let (mut tx, _rx) = conn.open_bi().await?;
        let packet = CRelayPacket::DrainAuth { timestamp, sig: Bytes::from(sig.to_bytes()) };
        packet.send(&mut tx).await?;
        _ = tx.finish();
        Ok(())
    }

    /// Batch-acknowledge a completed drain. The relay deletes the queue
    /// entries it streamed and — when some came from remote homes —
    /// replies with an `AckAuthRequest` on this stream's response half,
    /// asking us to sign the home-side GC. The signed `AckAuth` reply
    /// must go on a FRESH stream: the relay's dispatcher for this
    /// stream is parked inside `handle_ack_drain` awaiting the parked
    /// oneshot, so it can't read a reply from the same stream.
    async fn ack_drain(&self, conn: &quinn::Connection, ipk: VerifyingKey) {
        let (mut tx, mut rx) = match conn.open_bi().await {
            Ok(s) => s,
            Err(e) => {
                warn!("relay({}) ack_drain: open_bi failed: {e}", self.id);
                return;
            },
        };
        if CRelayPacket::AckDrain.send(&mut tx).await.is_err() {
            return;
        }
        _ = tx.finish();

        // Optional follow-up: absent when the drain was local-only (the
        // relay's stream task just ends → read errors out).
        if let Ok(Ok(SRelayPacket::AckAuthRequest {
            requester_relay_id,
            delivered_ids,
            suggested_timestamp,
        })) = tokio::time::timeout(Duration::from_secs(10), SRelayPacket::unpack(&mut rx)).await
        {
            match conn.open_bi().await {
                Ok((mut ack_tx, _ack_rx)) => {
                    if let Err(e) = handle_ack_auth_request(
                        &mut ack_tx,
                        ipk,
                        requester_relay_id,
                        delivered_ids,
                        suggested_timestamp,
                    )
                    .await
                    {
                        warn!("relay({}) ack_drain: AckAuth reply failed: {e}", self.id);
                    }
                    _ = ack_tx.finish();
                },
                Err(e) => {
                    warn!("relay({}) ack_drain: open_bi for AckAuth failed: {e}", self.id)
                },
            }
        }
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

        // Re-dispatch durably-queued outbox rows (enqueued while offline, or
        // whose ack was lost) now that a live relay connection exists.
        // Spawned so it never blocks the welcome-poll / drain / accept loop.
        tokio::spawn(async { crate::delivery::reconcile().await });

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        // Re-use the production peer/1 dialer that `connect()` built
        // before storing this Relay on the global `RELAY`. The dialer
        // is shared (`Arc<RelayDhtClient>`) so the uniffi surface
        // (`send_message`) and the background tasks below all dispatch
        // over the same pool.
        //
        // The cancellation token is fired when the function returns
        // (on connection loss) so the scheduler task exits cleanly.
        // `_mls_cancel_drop_guard` keeps the cancel-on-drop alive
        // through the rest of `handle()` — see line below.
        let mls_cancel = CancellationToken::new();
        let dht_client = self.dht_client.clone();

        if let Some(client) = dht_client.as_ref() {
            // Welcome poll — awaited BEFORE the queue drain so a pairing
            // Welcome is processed before the first application message
            // that references its group is drained (a drained message
            // from a not-yet-known sender would be dropped). Bounded so
            // a dead DHT can't stall the drain.
            match tokio::time::timeout(Duration::from_secs(15), poll_welcomes_once(client.clone()))
                .await
            {
                Ok(Ok(())) => {},
                Ok(Err(e)) => warn!("MLS: poll_welcomes failed: {e}"),
                Err(_) => warn!("MLS: poll_welcomes timed out; draining anyway"),
            }

            // Durable first-send retry: re-drive first-sends that deferred
            // (peer had no published KP). Outbound-only, so it needn't gate the
            // inbound drain or the Connected state — spawned (not awaited) so its
            // DHT round-trip doesn't stretch the "Syncing…" window. Spawned HERE,
            // after the welcome poll returned, so a Welcome that just paired us is
            // still applied before we retry a first-send to that peer (no fork).
            // ponytail: 15s cap matches poll_welcomes.
            let client_for_retry = client.clone();
            tokio::spawn(async move {
                if tokio::time::timeout(
                    Duration::from_secs(15),
                    retry_pending_sends_once(client_for_retry),
                )
                .await
                .is_err()
                {
                    warn!("MLS: retry_pending_sends timed out");
                }
            });

            // KP rotation scheduler — long-lived task, ticks every
            // KP_SCHEDULER_TICK_MS. Cancelled on disconnect via
            // `mls_cancel`.
            let client_for_sched = client.clone();
            let cancel_for_sched = mls_cancel.clone();
            tokio::spawn(async move {
                run_scheduler_loop(client_for_sched, cancel_for_sched).await;
            });
        }

        //==:==:==:==:==:==:==:==:==:==:==:==:==:==:==||

        // Drain the offline queue. The relay streams every queued
        // message back as individual `Deliver` frames on this stream's
        // response half; processing is store-only. Per-message
        // `DeliverAck` is the live-delivery contract — the drain is
        // acknowledged as one batch via `AckDrain` once everything is
        // durably stored.
        {
            let (mut tx, mut rx) =
                ret_err!(conn.open_bi().await.inspect_err(|e| self.handle_err(e)));

            if CRelayPacket::DrainQueue.send(&mut tx).await.is_err() {
                return ConnectionError::LocallyClosed;
            }
            _ = tx.finish();

            let mut received = 0usize;
            let mut failed = false;
            while let Ok(packet) = SRelayPacket::unpack(&mut rx).await {
                match packet {
                    SRelayPacket::Deliver(msg) => {
                        received += 1;
                        if let Err(e) = process_deliver(msg, self.dht_client.clone()).await {
                            failed = true;
                            warn!("relay({}) drain: retaining message for retry: {e}", self.id);
                        }
                    },
                    other => debug!("unexpected packet in drain response: {other:?}"),
                }
            }

            if received > 0 && !failed {
                info!("relay({}): drained {received} queued message(s)", self.id);
                self.ack_drain(conn, ipk).await;
            }
        }

        // Offline backlog is in the local DB — synced and live. (A drain-setup
        // failure returns above → Disconnected, so we never stick on Syncing.)
        ConnectionState::Connected.emit();

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
            // Clone the dialer Arc into the per-stream task so
            // `handle_deliver` can drive `process_inbound_envelope` over
            // the production wire (KP fetch on stale-group recreate,
            // etc.) instead of the stub `NotWiredDhtClient`.
            let dht_client_for_stream = self.dht_client.clone();
            tokio::spawn(async move {
                let _permit = permit; // dropped when stream task ends
                while let Ok(packet) = SRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = match packet {
                        SRelayPacket::Deliver(msg) => {
                            handle_deliver(&mut send, ipk, msg, dht_client_for_stream.clone()).await
                        },
                        SRelayPacket::Activity(eph) => {
                            handle_activity(ipk, eph);
                            Ok(())
                        },
                        SRelayPacket::Presence(list) => {
                            handle_presence(list);
                            Ok(())
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

/// Live-delivery entry point: process one relay-initiated `Deliver`,
/// then ack on the stream — the relay's `try_deliver` waits on this
/// ack before treating the message as delivered.
async fn handle_deliver(
    tx: &mut SendStream, _ipk: VerifyingKey, msg: DeliverP, dht_client: Option<Arc<RelayDhtClient>>,
) -> Result<()> {
    process_deliver(msg, dht_client).await?;
    CRelayPacket::DeliverAck.send(tx).await?;
    Ok(())
}

/// Process an inbound ephemeral signal (presence/typing): verify it's addressed
/// to us and authentically signed by a known contact, then surface it. Never
/// stored — a forged or stranger signal is dropped silently.
fn handle_activity(our_ipk: VerifyingKey, eph: common::proto::client_rel::ActivityP) {
    if eph.to.as_slice() != our_ipk.as_bytes().as_slice() {
        return;
    }
    let Ok(vk) = VerifyingKey::from_bytes(&eph.from.0) else { return };
    let transcript = common::proto::client_rel::activity_sig_message(
        &eph.to.0,
        &eph.from.0,
        eph.activity,
        eph.timestamp,
    );
    if vk.verify_strict(&transcript, &ed25519_dalek::Signature::from_bytes(&eph.sig.0)).is_err() {
        return;
    }
    if !Contact::exists(&eph.from.0) {
        return;
    }
    crate::events::messaging::ActivityEv { peer: eph.from.0, activity: eph.activity }.emit();
}

/// Surface a relay-asserted presence push (snapshot or delta). Relay-trusted
/// (no sig — the relay is the presence authority), but we still drop entries
/// for non-contacts as defense-in-depth.
fn handle_presence(list: Vec<common::proto::client_rel::PresenceP>) {
    use common::proto::client_rel::PresenceState;

    use crate::platform::Presence;
    for e in list {
        if !Contact::exists(&e.who.0) {
            continue;
        }
        let presence = match e.state {
            PresenceState::Online => Presence::Online,
            PresenceState::Idle { since } => Presence::Idle { since },
            PresenceState::Offline { last_seen } => Presence::Offline { last_seen },
        };
        crate::events::messaging::PresenceEv { peer: e.who.0, presence }.emit();
    }
}

/// Decode, decrypt, persist, and surface one delivered message.
/// Stream-free so both delivery channels share it: the live path
/// (`handle_deliver`, per-message ack) and the DrainQueue response
/// stream (batch-acked via `AckDrain`). `Ok(())` means the message
/// reached a terminal state (stored / buffered / correctly dropped);
/// `Err` means it was dropped without effect.
/// True if `payload` is an `MlsEnvelopeP::Welcome`. Pure → gate is testable.
fn is_welcome_envelope(payload: &[u8]) -> bool {
    matches!(
        common::proto::mls_wire::MlsEnvelopeP::deser(payload),
        Ok(common::proto::mls_wire::MlsEnvelopeP::Welcome(_))
    )
}

async fn process_deliver(msg: DeliverP, dht_client: Option<Arc<RelayDhtClient>>) -> Result<()> {
    // Already decrypted on an earlier connection? A different home is
    // redelivering. Ack (Ok → relay GCs) but NEVER re-decrypt: the ratchet
    // key is spent and openmls would SecretReuseError. Outer-keyed + pre-
    // decrypt, so it covers text, control, and welcome alike.
    if crate::data::seen::Seen::contains(&msg.from, &msg.id.0) {
        return Ok(());
    }

    // The wire envelope is `MlsEnvelopeP` (postcard-encoded), so we
    // hand off to `api::messaging::process_inbound_envelope` rather
    // than the v2 shared-key decrypt.
    //
    // Drop Application envelopes from unknown senders. A Welcome from a
    // stranger is a legit first-pair — let it reach the invite gate downstream.
    if !is_welcome_envelope(&msg.payload) && !Contact::exists(&msg.from) {
        info!("MESSAGE: dropped envelope from unknown sender {}", hex::encode(&msg.from[..4]));
        bail!("unknown sender");
    }

    // Use the production peer/1 dialer that the connection-time wiring
    // in `Relay::connect` attached to the global `RELAY`. The receive
    // path's MLS handling
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
            let ctx = crate::messaging::MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      client.as_ref(),
            };
            crate::messaging::process_inbound_envelope(&ctx, *msg.from, &msg.payload).await
        },
        None => {
            let dht = crate::quic::dht_client::NotWiredDhtClient;
            let ctx = crate::messaging::MlsContext {
                provider: &provider,
                stash:    &stash,
                buffer:   &buffer,
                dht:      &dht,
            };
            crate::messaging::process_inbound_envelope(&ctx, *msg.from, &msg.payload).await
        },
    };

    match result {
        Ok(Some(crate::messaging::InboundDecoded::Application { plaintext, group_id: _ })) => {
            // Decrypt succeeded → ratchet advanced. Record now (before the
            // payload sub-match) so any redelivery is caught pre-decrypt,
            // even if a downstream save fails — the ratchet key is spent
            // either way.
            crate::data::seen::Seen::record(&msg.from, &msg.id.0, systime().as_secs());
            // Proof of pair (PAIRING.md): a decryptable inbound message means
            // the group works, so a PENDING contact is now confirmed. No-op if
            // already paired. Fires for PairAck and any real message alike.
            Contact::mark_paired(&msg.from);
            match AppPayload::deser(&plaintext) {
                // Reply is Text + the quoted message's dispatch_id.
                Ok(p @ (AppPayload::Text(..) | AppPayload::Reply { .. })) => {
                    let (content, reply_to) = match p {
                        AppPayload::Text(c) => (c, None),
                        AppPayload::Reply { reply_to, content } => (content, Some(reply_to)),
                        _ => unreachable!(),
                    };
                    let timestamp = msg.accepted_at_ms / 1_000;
                    match Message::save_incoming(
                        *msg.from, &msg.id.0, &content, timestamp, reply_to,
                    ) {
                        Ok(Some(saved)) => {
                            MessageEv::Received {
                                id: saved.inner.id,
                                from: *msg.from,
                                content,
                                timestamp,
                            }
                            .emit();
                            info!("MESSAGE: received from {}", hex::encode(&msg.from[..4]));
                            // Auto-Delivered receipt (high-water-mark = this id).
                            // Spawned so we don't delay the relay's DeliverAck.
                            let from = *msg.from;
                            let upto = msg.id.0;
                            crate::RUNTIME.spawn(async move {
                                let _ = crate::messaging::send_receipt(
                                    from,
                                    ReceiptKind::Delivered,
                                    upto,
                                )
                                .await;
                            });
                        },
                        // Relay redelivered a dispatch_id we already stored: no
                        // re-emit, but still Ok so the caller acks and the relay GCs.
                        Ok(None) => {
                            debug!(
                                "MESSAGE: duplicate from {}, already stored",
                                hex::encode(&msg.from[..4])
                            );
                        },
                        Err(e) => {
                            warn!("MESSAGE: failed to save incoming: {e}");
                            bail!("save failed: {e}");
                        },
                    }
                },
                Ok(AppPayload::Receipt { kind, upto }) => {
                    let status = match kind {
                        ReceiptKind::Delivered => crate::data::message::STATUS_DELIVERED,
                        ReceiptKind::Read => crate::data::message::STATUS_READ,
                    };
                    if Message::mark_receipt_upto(&msg.from, &upto, status) {
                        MessageEv::Receipt { peer: *msg.from, upto, status }.emit();
                    }
                },
                Ok(AppPayload::Edit { target, content }) => {
                    // own=false: a peer may only edit messages IT sent us (outgoing=0).
                    match Message::apply_edit(&msg.from, &target, &content, false) {
                        Some(row) => {
                            info!("MESSAGE: edit from {}", hex::encode(&msg.from[..4]));
                            MessageEv::Edited { id: row.id, peer: *msg.from, content }.emit();
                        },
                        // Out-of-order: target not stored yet. Rare in 1:1
                        // same-epoch (the original precedes) — drop.
                        None => debug!(
                            "MESSAGE: edit for unknown target from {}",
                            hex::encode(&msg.from[..4])
                        ),
                    }
                },
                Ok(AppPayload::Delete { target }) => {
                    // own=false: a peer may only delete messages IT sent us (outgoing=0).
                    match Message::apply_delete(&msg.from, &target, false) {
                        Some(row) => {
                            info!("MESSAGE: delete from {}", hex::encode(&msg.from[..4]));
                            MessageEv::Deleted { id: row.id, peer: *msg.from }.emit();
                        },
                        None => debug!(
                            "MESSAGE: delete for unknown target from {}",
                            hex::encode(&msg.from[..4])
                        ),
                    }
                },
                Ok(AppPayload::React { target, emoji, add }) => {
                    // Reactor is the MLS sender (`msg.from`) — attributed to its
                    // own IPK, so this is already group-correct.
                    let ts = systime().as_secs();
                    if crate::data::reaction::Reaction::apply(
                        &msg.from, &target, &msg.from, &emoji, add, ts,
                    ) {
                        crate::events::messaging::ReactionEv {
                            peer: *msg.from,
                            dispatch_id: target,
                            reactor: *msg.from,
                            emoji,
                            add,
                        }
                        .emit();
                    }
                },
                Ok(AppPayload::PairAck) => {
                    // Proof-of-pair — its whole job was the mark_paired above.
                    info!("PAIR: confirmed by {}", hex::encode(&msg.from[..4]));
                },
                Ok(AppPayload::P2p { candidates, relay, token, disco_key }) => {
                    // Candidate offer for a direct connection — hand to the
                    // P2P layer (routed to the waiting session), never stored.
                    info!(
                        "P2P: received offer from {} ({} candidates)",
                        hex::encode(&msg.from[..4]),
                        candidates.len()
                    );
                    crate::p2p::deliver_offer(*msg.from, candidates, relay, token, disco_key);
                },
                Ok(AppPayload::Image { caption, group_id, mime, width, height, data }) => {
                    let did = msg.id.0;
                    let timestamp = msg.accepted_at_ms / 1_000;
                    let media = crate::data::media::MediaRow {
                        kind: crate::data::media::KIND_IMAGE,
                        group_id: group_id.map(|g| g.to_vec()),
                        mime, name: String::new(), size: data.len() as u64, width, height,
                        blob: Some(data), thumb: None, file_id: None,
                    };
                    if let Some(saved) = crate::data::media::save_incoming_with_media(
                        &msg.from, &did, &caption, timestamp, &media,
                    )? {
                        MessageEv::Received {
                            id: saved.inner.id,
                            from: *msg.from,
                            content: caption,
                            timestamp,
                        }
                        .emit();
                        info!("MESSAGE: received image from {}", hex::encode(&msg.from[..4]));
                        let from = *msg.from;
                        let upto = did;
                        crate::RUNTIME.spawn(async move {
                            let _ = crate::messaging::send_receipt(from, ReceiptKind::Delivered, upto).await;
                        });
                    }
                },
                Ok(AppPayload::Attachment { caption, group_id, mime, name, size, thumb, file_id }) => {
                    let did = msg.id.0;
                    let timestamp = msg.accepted_at_ms / 1_000;
                    let media = crate::data::media::MediaRow {
                        kind: crate::data::media::KIND_ATTACHMENT,
                        group_id: group_id.map(|g| g.to_vec()),
                        mime, name, size, width: 0, height: 0,
                        blob: None,
                        thumb: if thumb.is_empty() { None } else { Some(thumb) },
                        file_id: Some(file_id.to_vec()),
                    };
                    if let Some(saved) = crate::data::media::save_incoming_with_media(
                        &msg.from, &did, &caption, timestamp, &media,
                    )? {
                        MessageEv::Received {
                            id: saved.inner.id,
                            from: *msg.from,
                            content: caption,
                            timestamp,
                        }
                        .emit();
                        info!("MESSAGE: received attachment from {}", hex::encode(&msg.from[..4]));
                        let from = *msg.from;
                        let upto = did;
                        crate::RUNTIME.spawn(async move {
                            let _ = crate::messaging::send_receipt(from, ReceiptKind::Delivered, upto).await;
                        });
                    }
                },
                Ok(AppPayload::FileWant { file_id }) => {
                    // Reverse-wake control message — routed, never stored. The push
                    // wake already revived us; bring the P2P listener up so the
                    // receiver's retry-dial can land (they drive the connect).
                    info!("P2P: FileWant received from {}", hex::encode(&msg.from[..4]));
                    crate::transfer::on_file_want(*msg.from, file_id);
                },
                Err(e) => {
                    warn!(
                        "MESSAGE: undecodable AppPayload from {}: {e}",
                        hex::encode(&msg.from[..4])
                    );
                    bail!("bad AppPayload");
                },
            }
        },
        Ok(Some(crate::messaging::InboundDecoded::Welcome)) => {
            crate::data::seen::Seen::record(&msg.from, &msg.id.0, systime().as_secs());
            info!("MLS: processed welcome from {}", hex::encode(&msg.from[..4]));
            // Accepting the welcome built the group → prove it works back to
            // the inviter so their contact flips PENDING → PAIRED.
            let to = *msg.from;
            crate::RUNTIME.spawn(async move {
                if let Err(e) = crate::messaging::send_pair_ack(to).await {
                    warn!("PAIR: ack send to {} failed: {e}", hex::encode(&to[..4]));
                }
            });
        },
        Ok(Some(crate::messaging::InboundDecoded::WelcomeRejected { sender_ipk, reason })) => {
            crate::data::seen::Seen::record(&msg.from, &msg.id.0, systime().as_secs());
            warn!("PAIR: could not accept welcome from {}; declining", hex::encode(&msg.from[..4]));
            crate::RUNTIME.spawn(async move {
                if let Err(e) = crate::messaging::send_pair_decline(sender_ipk, reason).await {
                    warn!("PAIR: decline send failed: {e}");
                }
            });
        },
        Ok(Some(crate::messaging::InboundDecoded::PairDeclined)) => {
            crate::data::seen::Seen::record(&msg.from, &msg.id.0, systime().as_secs());
            // Already applied (contact REJECTED, messages failed) — just ack.
        },
        Ok(Some(crate::messaging::InboundDecoded::ApplicationBuffered)) => {
            // Buffered for a future epoch / staged commit merged.
            // Terminal-good: the caller acks so the relay GCs the entry.
        },
        Ok(Some(crate::messaging::InboundDecoded::ApplicationNoGroup { .. })) => {
            // No local group state (post-restore): the ciphertext is
            // unrecoverable, so ack and let the relay GC it — refusing to
            // ack meant redelivery forever. messaging already fired the
            // re-establishment toward the (known-contact) sender.
            warn!(
                "MESSAGE: dropped message for dead group from {}; re-establishment fired",
                hex::encode(&msg.from[..4])
            );
        },
        Ok(Some(crate::messaging::InboundDecoded::ApplicationStale)) => {
            // Ack stale-epoch envelopes so the relay GCs them.
            // Previously this `bail`ed without ack which made
            // the relay redeliver indefinitely (queue grows without
            // bound, CPU burns on every redelivery decoding the same
            // doomed envelope). The recipient cannot recover state for
            // a stale epoch — openmls only retains a small past-epoch
            // key window — so re-delivery is hopeless, and an explicit
            // ack is the correct response.
            warn!("MESSAGE: stale-epoch envelope from {}; dropping", hex::encode(&msg.from[..4]));
        },
        Ok(Some(crate::messaging::InboundDecoded::ApplicationUndecryptable)) => {
            // Sender-ratchet secret is permanently unavailable. Returning Ok
            // lets live delivery and queue draining acknowledge this envelope.
        },
        Ok(None) => {
            // Currently unreachable — process_inbound_envelope only
            // returns None for the protocol-mismatch path, which
            // never fires in production.
            bail!("no inbound action");
        },
        Err(e) => {
            warn!(
                "MESSAGE: process_inbound_envelope failed from {}: {e}",
                hex::encode(&msg.from[..4])
            );
            bail!("process failed: {e}");
        },
    }

    Ok(())
}

/// Handle a relay-issued `SRelayPacket::AckAuthRequest`.
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
/// **`requester_relay_id` binding**: the relay supplies its own NodeId
/// via `requester_relay_id`; we sign that value verbatim into the
/// transcript. The home cross-checks the field
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
// MLS / DHT-RPC dialer wiring
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
fn build_relay_dht_client(relay: &Relay, ipk: VerifyingKey) -> Result<Arc<RelayDhtClient>> {
    let conn =
        relay.connection.clone().ok_or_else(|| anyhow!("relay connection not established"))?;
    Ok(Arc::new(RelayDhtClient::new(conn, ipk.to_bytes(), relay.home_node_id)))
}

/// One-shot Welcome poll on reconnect. Builds an `MlsContext` against
/// fresh DB handles and the supplied dialer; runs
/// [`crate::messaging::poll_welcomes`] once.
async fn poll_welcomes_once(client: Arc<RelayDhtClient>) -> Result<()> {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = stash_db_handle();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = crate::messaging::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    let count = crate::messaging::poll_welcomes(&ctx).await?;
    if count > 0 {
        info!("MLS: poll_welcomes processed {count} welcome(s)");
    }
    Ok(())
}

/// Reconnect hook for durable first-send: builds a production
/// [`crate::messaging::MlsContext`] (fresh DB handles + the connection's
/// dialer, mirroring `poll_welcomes_once`) and re-drives every still-
/// pending first-send whose contact has no group yet — the ones deferred
/// earlier because the peer had no published KeyPackage.
async fn retry_pending_sends_once(client: Arc<RelayDhtClient>) {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = stash_db_handle();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = crate::messaging::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    crate::messaging::retry_pending_sends(&ctx).await;
}

/// KP-rotation scheduler loop — production wiring.
///
/// Loads the user's identity + signing key from the libcore globals,
/// then delegates to [`run_scheduler_inner`]. Errors loading the
/// identity exit the loop early (logged); the inner loop owns the
/// cancellation contract.
async fn run_scheduler_loop(client: Arc<RelayDhtClient>, cancel: CancellationToken) {
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
    // Republish our KP to this relay on connect (idempotent) — fixes the case where
    // the relay lost our KP but our local stash is still full so `should_refill` never fires.
    crate::mls::scheduler::ensure_kp_published(&provider, &stash, &signing, client.as_ref()).await;
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
async fn run_scheduler_inner<C: crate::quic::dht_client::DhtClient>(
    provider: &crate::mls::PromtuzMlsProvider, stash: &crate::mls::KeyPackageStash,
    signing: &ed25519_dalek::SigningKey, dht: &C, tick_interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        let now_ms = systime().as_millis() as u64;
        match crate::mls::scheduler::run_once(provider, stash, signing, dht, now_ms).await {
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
    // ----- KP scheduler tokio task tests -----------------------------

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
        assert_eq!(dht.published_kp_batches.lock().len(), 1, "healthy-stash tick is NoOp");

        // Cancel and confirm the loop exits.
        cancel.cancel();
        // Give it a chance to observe cancel + return.
        tokio::time::advance(Duration::from_millis(1)).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("scheduler exits within 1s of cancel");
    }
}

#[cfg(test)]
mod gate_tests {
    use common::proto::mls_wire::MlsEnvelopeP;
    use common::proto::mls_wire::WelcomeEnvelopeP;
    use common::proto::pack::Packer;

    use super::is_welcome_envelope;

    #[test]
    fn welcome_envelope_bypasses_contact_gate() {
        // A garbage / non-Welcome payload must stay gated (returns false).
        assert!(!is_welcome_envelope(b"not an envelope"), "garbage must stay gated");
        // A real Welcome envelope must be recognized so it bypasses the gate.
        let env = WelcomeEnvelopeP {
            version:       0,
            group_id:      [0u8; 32].into(),
            sender_ipk:    [0u8; 32].into(),
            recipient_ipk: [0u8; 32].into(),
            welcome_blob:  common::types::bytes::ByteVec(vec![9, 9, 9]),
            kp_ref_used:   [0u8; 32].into(),
            sender_sig:    [0u8; 64].into(),
            pairing:       None,
        };
        let bytes = MlsEnvelopeP::Welcome(env).ser().expect("ser");
        assert!(is_welcome_envelope(&bytes), "a Welcome envelope must bypass the contact gate");
    }
}
