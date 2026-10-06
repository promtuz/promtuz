use std::collections::HashSet;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::Sender;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::SHandshakePacket as SHSP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::ServerHandshakeResultP as SHSRP;
use common::proto::dht_p2p::MAX_FETCH_QUEUE_ACK_IDS;
use common::proto::dht_p2p::queue_fetch_ack_signing_input;
use common::proto::dht_p2p::queue_fetch_signing_input;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use ed25519_dalek::VerifyingKey;
use log::debug;
use log::error;
use log::info;
use log::warn;
use quinn::ConnectionError;
use quinn::SendStream;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::data::identity::IdentitySigner;
use crate::data::relay::Relay;
use crate::events::Emittable;
use crate::events::connection::ConnectionState;
use crate::messaging::receive::process_deliver;
use crate::mls::scheduler::run_scheduler_loop;
use crate::presence::handle_activity;
use crate::presence::handle_presence;
use crate::quic::dht_client::DhtClient;
use crate::quic::relay_dht_client::RelayDhtClient;
use crate::state::Core;
use crate::state::core;
use crate::utils::addr_short;
use crate::utils::node_short;

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

const MAX_CONCURRENT_STREAMS: usize = 16;
const RTT_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Below half of `PRESENCE_LEASE_MAX_MS`, so a single missed renewal leaves the
/// claim standing and only a real departure lets it lapse.
const PRESENCE_RENEW_INTERVAL: Duration = Duration::from_secs(4 * 60);

/// EOF between frames ends a drain; truncation and transport errors do not.
async fn read_relay_packet<R: AsyncRead + Unpin + Send>(
    rx: &mut R,
) -> Result<Option<SRelayPacket>> {
    let mut first = [0u8; 1];
    if rx.read(&mut first).await? == 0 { return Ok(None); }
    let mut framed = first.as_slice().chain(rx);
    Ok(Some(SRelayPacket::unpack(&mut framed).await?))
}

struct InboxSync {
    connection: quinn::Connection,
    completed: bool,
}

impl Drop for InboxSync {
    fn drop(&mut self) {
        if !self.completed {
            // A cancelled worker must not leave an old drain mutating the
            // relay's pending batch while its replacement starts another.
            self.connection.close(0u32.into(), b"inbox-sync-interrupted");
        }
    }
}

/// The same channel-bound authentication is used by messaging and pinned profile reads.
async fn authenticate(conn: &quinn::Connection, ipk: VerifyingKey) -> Result<SHSRP> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let (mut tx, mut rx) = conn.open_bi().await?;

        CHandshakePacket::Hello { ipk: ipk.to_bytes().into() }.send(&mut tx).await?;

        let SHSP::Challenge { nonce } = SHSP::unpack(&mut rx).await? else {
            bail!("Handshake Packet Order Mismatch");
        };

        let binding = common::quic::client_auth_binding(conn)?;
        let msg = common::proto::client_rel::client_auth_message(&nonce, &binding);

        CHandshakePacket::Proof { sig: IdentitySigner::sign(&msg)?.to_bytes().into() }
            .send(&mut tx)
            .await?;

        let SHSP::HandshakeResult(result) = SHSP::unpack(&mut rx).await? else {
            bail!("Handshake Packet Order Mismatch");
        };

        Ok(result)
    })
    .await?
}

impl Relay {
    pub async fn connect(
        mut self, ipk: VerifyingKey,
    ) -> Result<JoinHandle<ConnectionError>, RelayConnError> {
        let addr = SocketAddr::new(IpAddr::from_str(&self.host)?, self.port);

        info!("connecting to relay {} ({})", node_short(&self.id), addr_short(addr));
        ConnectionState::Connecting.emit();

        // Wait before dialing: the relay's authentication deadline starts when it accepts the
        // transport. Keep profile logins excluded until the primary session is published.
        let profile_update = core().profile_update.lock().await;
        let conn = match crate::quic::dialer::connect(addr, &self.id).await {
            Ok(conn) => conn,
            Err(err) => {
                ConnectionState::Failed.emit();
                if err.is_security() {
                    warn!(
                        "relay {} ({}) cert/auth failure ({err}) — terminal, will not retry",
                        node_short(&self.id),
                        addr_short(addr)
                    );
                    _ = self.record_terminal_failure();
                } else {
                    error!(
                        "relay {} ({}) connect failed: {err}",
                        node_short(&self.id),
                        addr_short(addr)
                    );
                    _ = self.record_failure();
                }
                return Err(RelayConnError::Continue);
            },
        };

        ConnectionState::Handshaking.emit();

        let result = authenticate(&conn, ipk).await.map_err(RelayConnError::Error)?;

        let (home_node_id, turn_port) = match result {
            SHSRP::Accept { relay_node_id, assist, turn_port, .. } => {
                // Remembered on the row too, so a later session can pick an
                // assist-capable relay without being connected to it.
                self.assist = assist;
                if let Err(e) = self.record_assist(assist) {
                    warn!("relay {} assist flag not recorded: {e}", node_short(&self.id));
                }
                (relay_node_id.map(|b| b.0), turn_port)
            },
            SHSRP::Reject { reason } => {
                warn!("relay handshake failed : {reason}");
                _ = self.record_failure();
                return Err(RelayConnError::Continue);
            },
        };

        info!("authenticated with relay {}", node_short(&self.id));
        // Auth is up but the offline backlog is not drained yet. `handle` emits Connected once it
        // is, and failures below emit Disconnected, so this state never sticks.
        ConnectionState::Syncing.emit();

        self.record_success().map_err(|e| RelayConnError::Error(e.into()))?;

        let core = core();
        let session = Arc::new(Session::new(core, self, conn, ipk, home_node_id, turn_port));
        // Published before `handle` starts, so the work it spawns finds this connection.
        core.publish(session.clone());
        drop(profile_update);

        session.spawn({
            let session = session.clone();
            async move { session.sample_rtt().await }
        });

        let handle = session.spawn({
            let session = session.clone();
            async move {
                let error = session.handle(ipk).await;
                session.handle_err(&error);
                error
            }
        });

        // Presence is a lease, so a crash reads Offline on its own. It is renewed below half-life,
        // and only while the user is in the app: a wake drain holds no claim to extend.
        session.spawn({
            let session = session.clone();
            async move {
                let mut tick = tokio::time::interval(PRESENCE_RENEW_INTERVAL);
                tick.tick().await;
                while session.conn.close_reason().is_none() {
                    tokio::select! {
                        _ = tick.tick() => {},
                        _ = session.cancel.cancelled() => return,
                    }
                    if !crate::presence::presence_is_active() {
                        continue;
                    }
                    if let Err(e) = crate::presence::renew_presence(&session).await {
                        debug!("presence renewal failed: {e}");
                    }
                }
            }
        });

        // The relay treats a reconnect as Offline, so reassert presence first, ahead of push
        // registration, which can stall.
        session.spawn({
            let session = session.clone();
            async move {
                if let Err(e) = crate::presence::reassert_presence(&session).await {
                    debug!("PRESENCE: reassert on connect failed: {e}");
                }
                crate::push::request_registration();
            }
        });

        Ok(handle)
    }
}

/// An authenticated connection to the home relay and the work that belongs to it. Its token is
/// cancelled once the connection is lost.
pub struct Session {
    pub relay:        Relay,
    pub conn:         quinn::Connection,
    pub dht:          Arc<RelayDhtClient>,
    /// Relay storage identity from the handshake; older relays may omit it without DHT.
    pub home_node_id: Option<[u8; 32]>,
    /// UDP port of the relay's call TURN server.
    pub turn_port:    Option<u16>,
    pub cancel:       CancellationToken,
    pub tasks:        TaskTracker,
    parent:           TaskTracker,
    runtime:          tokio::runtime::Handle,
    /// Held by the outbox pass in progress.
    pub(crate) reconciling: Mutex<()>,
    services: tokio::sync::OnceCell<common::contracts::Support>,
    pub(crate) profile_registered: tokio::sync::OnceCell<()>,
}

impl Session {
    pub fn new(
        core: &Core, relay: Relay, conn: quinn::Connection, ipk: VerifyingKey,
        home_node_id: Option<[u8; 32]>, turn_port: Option<u16>,
    ) -> Self {
        Self {
            dht: Arc::new(RelayDhtClient::new(conn.clone(), ipk.to_bytes(), home_node_id)),
            relay,
            conn,
            home_node_id,
            turn_port,
            cancel: core.cancel.child_token(),
            tasks: TaskTracker::new(),
            parent: core.tasks.clone(),
            runtime: core.runtime.clone(),
            reconciling: Mutex::new(()),
            services: tokio::sync::OnceCell::new(),
            profile_registered: tokio::sync::OnceCell::new(),
        }
    }

    /// A short connection to the existing profile host. Does not publish a primary session,
    /// drain messages, assert presence, or emit connection-state changes. Caller holds
    /// profile_update until this connection is closed.
    pub(crate) async fn connect_profile(relay: Relay, ipk: VerifyingKey) -> Result<Self> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let addr = SocketAddr::new(IpAddr::from_str(&relay.host)?, relay.port);
            let conn = crate::quic::dialer::connect(addr, &relay.id).await?;
            let SHSRP::Accept { relay_node_id, turn_port, .. } = authenticate(&conn, ipk).await? else {
                conn.close(0u32.into(), b"profile-auth-rejected");
                bail!("profile host rejected authentication");
            };
            Ok(Self::new(core(), relay, conn, ipk, relay_node_id.map(|id| id.0), turn_port))
        })
        .await?
    }

    pub(crate) async fn services(&self) -> &common::contracts::Support {
        self.services.get_or_init(|| async {
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let (mut tx, mut rx) = self.conn.open_bi().await?;
                CRelayPacket::ServiceCapabilities.send(&mut tx).await?;
                tx.finish()?;
                let SRelayPacket::ServiceCapabilities { supported } = SRelayPacket::unpack(&mut rx).await? else {
                    bail!("unexpected service capabilities response");
                };
                Ok::<_, anyhow::Error>(common::contracts::Support::decode(&supported.0)?)
            }).await;
            match result {
                Ok(Ok(support)) => support,
                _ => common::contracts::Support::default(),
            }
        }).await
    }

    /// Runs `task` on the runtime, counted by this session and by its core.
    pub fn spawn<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.parent.spawn_on(self.tasks.track_future(task), &self.runtime)
    }

    /// Feeds the relays page graph and `fetch_best`'s latency term until the connection closes.
    async fn sample_rtt(&self) {
        while self.conn.close_reason().is_none() {
            let rtt_ms = self.conn.rtt().as_millis() as u64;
            if let Err(e) = self.relay.record_rtt(rtt_ms) {
                warn!("relay {} rtt sample failed: {e}", node_short(&self.relay.id));
            }
            tokio::select! {
                _ = tokio::time::sleep(RTT_SAMPLE_INTERVAL) => {},
                _ = self.cancel.cancelled() => return,
            }
        }
    }

    /// Lets this relay pull our offline queue from the K closest homes. The transcript names no
    /// home, so one signature serves all K within the ±60 s skew window.
    async fn send_drain_auth(&self, tx: &mut quinn::SendStream, ipk: VerifyingKey) -> Result<()> {
        let timestamp = now_ms();
        let relay_node_id = NodeId::from_str(&self.relay.id)
            .map_err(|e| anyhow!("relay id {:?} not parseable as NodeId: {e:?}", self.relay.id))?;
        let self_ipk = ipk.to_bytes();
        let transcript = queue_fetch_signing_input(&self_ipk, &relay_node_id, timestamp);
        let sig = IdentitySigner::sign(&transcript)?;

        let packet = CRelayPacket::DrainAuth { timestamp, sig: Bytes::from(sig.to_bytes()) };
        packet.send(tx).await?;
        Ok(())
    }

    /// The relay may answer with an `AckAuthRequest` for remote homes. The signed reply needs a
    /// fresh stream: the relay's handler for this one is parked and cannot read it.
    async fn ack_drain(&self, ipk: VerifyingKey, drained: &HashSet<[u8; 16]>) -> Result<()> {
        let conn = &self.conn;
        let (mut tx, mut rx) = conn.open_bi().await?;
        CRelayPacket::AckDrain { ids: drained.iter().copied().collect() }.send(&mut tx).await?;
        tx.finish()?;
        let reply = tokio::time::timeout(Duration::from_secs(10), read_relay_packet(&mut rx)).await??;
        let Some(reply) = reply else { return Ok(()) };
        let SRelayPacket::AckAuthRequest { requester_relay_id, delivered_ids, suggested_timestamp } =
            reply
        else {
            bail!("unexpected drain acknowledgement");
        };
        let (mut ack_tx, _ack_rx) = conn.open_bi().await?;
        handle_ack_auth_request(
            &mut ack_tx, ipk, requester_relay_id, delivered_ids, suggested_timestamp, drained,
        ).await?;
        ack_tx.finish()?;
        // The original stream ends after the remote-home ack round. Wait
        // before another drain replaces that round's pending state.
        if tokio::time::timeout(Duration::from_secs(10), read_relay_packet(&mut rx)).await??.is_some() {
            bail!("unexpected packet after drain acknowledgement");
        }
        Ok(())
    }

    fn handle_err(&self, err: &ConnectionError) {
        if core().retire(self) {
            ConnectionState::Disconnected.emit();
        }

        error!("relay {} connection lost: {err}", node_short(&self.relay.id));
    }

    /// Used both on connect and by a platform background job. Draining and
    /// acknowledging must stay serialized because the relay tracks one batch.
    pub(crate) async fn sync_incoming(&self, ipk: VerifyingKey) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(45), self.sync_incoming_inner(ipk)).await?
    }

    async fn sync_incoming_inner(&self, ipk: VerifyingKey) -> Result<()> {
        let _guard = core().inbox_sync.lock().await;
        let conn = &self.conn;
        let mut sync = InboxSync { connection: conn.clone(), completed: false };
        // Poll Welcomes before the drain: a message for a group we have not joined yet would be
        // dropped. Bounded so a dead DHT cannot stall the drain.
        match tokio::time::timeout(Duration::from_secs(15), poll_welcomes_once(self.dht.clone()))
            .await
        {
            Ok(Ok(())) => {},
            Ok(Err(e)) => warn!("MLS: poll_welcomes failed: {e}"),
            Err(_) => warn!("MLS: poll_welcomes timed out; draining anyway"),
        }
        // Queued messages arrive as `Deliver` frames on this stream. Unlike live delivery, the
        // drain is acked as one batch via `AckDrain`, naming what was stored.
        let mut previous = HashSet::new();
        for _ in 0..16 {
            let (mut tx, mut rx) = conn.open_bi().await?;
            // The relay must install auth before handling DrainQueue.
            self.send_drain_auth(&mut tx, ipk).await?;
            CRelayPacket::DrainQueue.send(&mut tx).await?;
            tx.finish()?;

            // The relay later asks us to sign deletion of the ids it claims it delivered; this set
            // is what that claim is checked against.
            let mut drained: HashSet<[u8; 16]> = HashSet::new();
            while let Some(packet) = read_relay_packet(&mut rx).await? {
                match packet {
                    SRelayPacket::Deliver(msg) => {
                        let id = msg.id.0;
                        match process_deliver(ipk, msg, self.dht.as_ref()).await {
                            Ok(()) => {
                                drained.insert(id);
                            },
                            Err(e) => {
                                warn!("relay {} drain: retaining message for retry: {e}", node_short(&self.relay.id));
                            },
                        }
                    },
                    other => debug!("unexpected packet in drain response: {other:?}"),
                }
            }

            if drained.is_empty() {
                sync.completed = true;
                return Ok(());
            }
            if drained == previous { bail!("relay did not acknowledge the previous drain"); }
            info!("relay {}: drained {} queued message(s)", node_short(&self.relay.id), drained.len());
            self.ack_drain(ipk, &drained).await?;
            previous = drained;
        }
        bail!("more queued messages remain")
    }

    /// Runs until the connection is lost.
    async fn handle(self: &Arc<Self>, ipk: VerifyingKey) -> ConnectionError {
        let conn = &self.conn;
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS));
        let _cancel_on_return = self.cancel.drop_guard_ref();

        // Spawned so it never blocks the drain or the accept loop.
        self.spawn({
            let session = self.clone();
            async move { crate::delivery::reconcile(&session).await }
        });

        if let Err(e) = self.sync_incoming(ipk).await {
            warn!("relay {} inbox sync failed: {e:#}", node_short(&self.relay.id));
            conn.close(0u32.into(), b"inbox-sync-failed");
            return ConnectionError::LocallyClosed;
        }

        // After the Welcome poll, so a Welcome that just paired us applies before the retry.
        // Not awaited, so it does not stretch the Syncing window.
        let client = self.dht.clone();
        self.spawn(async move {
            if tokio::time::timeout(Duration::from_secs(15), retry_pending_sends_once(client))
                .await
                .is_err()
            {
                warn!("MLS: retry_pending_sends timed out");
            }
        });

        // After the drain, so a commit waiting in the queue is applied
        // before we ask a committer again or take its place.
        crate::groups::on_reconnect();

        self.spawn(crate::transfer::resume_incomplete_downloads());
        crate::data::receipts::schedule();

        self.spawn(run_scheduler_loop(self.dht.clone(), self.cancel.child_token()));

        // After the drain, so stored profile updates apply first. It never gates inbox readiness.
        self.spawn(crate::profile_sync::run(self.cancel.child_token()));

        ConnectionState::Connected.emit();

        loop {
            let (mut send, mut recv) = tokio::select! {
                accepted = conn.accept_bi() => match accepted {
                    Ok(streams) => streams,
                    Err(e) => return e,
                },
                _ = self.cancel.cancelled() => {
                    conn.close(0u32.into(), b"shutdown");
                    return ConnectionError::LocallyClosed;
                },
            };

            let permit = match semaphore.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    debug!("relay {} stream limit reached, dropping stream", node_short(&self.relay.id));
                    continue;
                },
            };

            let session = self.clone();
            self.spawn(async move {
                let _permit = permit;
                while let Ok(packet) = SRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = match packet {
                        SRelayPacket::Deliver(msg) => {
                            handle_deliver(&mut send, ipk, msg, session.dht.as_ref()).await
                        },
                        SRelayPacket::Activity(eph) => {
                            handle_activity(ipk, eph);
                            Ok(())
                        },
                        SRelayPacket::Presence(list) => {
                            handle_presence(list);
                            Ok(())
                        },
                        SRelayPacket::ProfileChanged { owner } => {
                            if crate::data::contact::Contact::is_paired(&owner.0) {
                                crate::profile_sync::store::invalidate(owner.0);
                            }
                            Ok(())
                        },
                        // An ack authorization only ever answers our own
                        // AckDrain, on the stream we opened for it.
                        SRelayPacket::AckAuthRequest { .. } => {
                            debug!("ignoring unsolicited AckAuthRequest");
                            Ok(())
                        },
                        other => {
                            debug!("unexpected packet from relay: {other:?}");
                            Ok(())
                        },
                    } {
                        warn!("relay {} handle err: {err}", node_short(&session.relay.id));
                    }
                }
            });
        }
    }
}

/// The relay treats the message as delivered only once this ack arrives.
async fn handle_deliver<C: DhtClient>(
    tx: &mut SendStream, ipk: VerifyingKey, msg: DeliverP, dht: &C,
) -> Result<()> {
    process_deliver(ipk, msg, dht).await?;
    CRelayPacket::DeliverAck.send(tx).await?;
    Ok(())
}

/// The signature authorises permanent deletion at every home, so it is only given for ids this
/// connection streamed. An unsolicited request has an empty `drained` and is refused.
async fn handle_ack_auth_request(
    tx: &mut SendStream, ipk: VerifyingKey, requester_relay_id: NodeId,
    delivered_ids: Vec<[u8; 16]>, suggested_timestamp: u64, drained: &HashSet<[u8; 16]>,
) -> Result<()> {
    if delivered_ids.len() > MAX_FETCH_QUEUE_ACK_IDS {
        warn!(
            "ACK_AUTH: delivered_ids overflow ({} > {}); dropping",
            delivered_ids.len(),
            MAX_FETCH_QUEUE_ACK_IDS
        );
        return Ok(());
    }
    if let Some(stray) = delivered_ids.iter().find(|id| !drained.contains(*id)) {
        warn!("ACK_AUTH: refusing to sign undelivered id {}", hex::encode(&stray[..4]));
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

async fn poll_welcomes_once(client: Arc<RelayDhtClient>) -> Result<()> {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = core().db.mls();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = crate::messaging::session::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    let count = crate::messaging::welcome::poll_welcomes(&ctx).await?;
    if count > 0 {
        info!("MLS: poll_welcomes processed {count} welcome(s)");
    }
    Ok(())
}

/// Re-drives pending first-sends deferred because the peer had no published KeyPackage.
pub(crate) async fn retry_pending_sends_once(client: Arc<RelayDhtClient>) {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    let stash_db = core().db.mls();
    let stash = crate::mls::KeyPackageStash::new(stash_db.clone());
    let buffer = crate::mls::EpochCatchupBuffer::new(stash_db);
    let ctx = crate::messaging::session::MlsContext {
        provider: &provider,
        stash:    &stash,
        buffer:   &buffer,
        dht:      client.as_ref(),
    };
    crate::messaging::send::retry_pending_sends(&ctx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ScopedCore, data, net};
    use common::quic::protorole::ProtoRole;

    /// A profile pass may outlast the relay's authentication deadline. Waiting for it must
    /// happen before any transport is opened, while still excluding temporary profile logins.
    #[tokio::test(start_paused = true)]
    async fn primary_waits_for_profile_before_opening_transport() {
        let _clock = net::step_paused_clock();
        let scope = ScopedCore::new();
        let me = data::identity(&scope.core.db.identity().lock(), 94);
        let (endpoint, roots) = net::server(ProtoRole::Client);
        let relay = Relay {
            id: "localhost".into(),
            host: "127.0.0.1".into(),
            port: endpoint.local_addr().unwrap().port(),
            pubkey: None,
            assist: false,
        };
        assert!(
            scope
                .core
                .net
                .set(crate::state::Net {
                    endpoint: net::client_endpoint(),
                    dialer: crate::quic::dialer::ClientConfigs {
                        quic: net::client_config(ProtoRole::Client, &roots),
                        roots,
                    },
                    seeds: vec![],
                })
                .is_ok()
        );
        scope.core.db.network().lock().execute(
            "INSERT INTO relays(id,host,port,protocol_version,window_start,last_seen) VALUES ('localhost','127.0.0.1',?1,?2,0,0)",
            (relay.port, common::PROTOCOL_VERSION),
        ).unwrap();
        let profile = scope.core.profile_update.lock().await;
        let connect = relay.connect(me.verifying_key());
        tokio::pin!(connect);
        tokio::select! {
            biased;
            _ = &mut connect => panic!("connection completed while a profile pass held the lock"),
            _ = endpoint.accept() => panic!("transport opened before the profile pass finished"),
            _ = tokio::time::sleep(Duration::from_secs(16)) => {},
        }
        assert!(scope.core.session().is_none());
        drop(profile);

        let server = async {
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                let (mut tx, mut rx) = conn.accept_bi().await.unwrap();
                assert!(matches!(
                    CHandshakePacket::unpack(&mut rx).await.unwrap(),
                    CHandshakePacket::Hello { .. }
                ));
                // Still exclude profile connections until the primary session is published.
                assert!(scope.core.profile_update.try_lock().is_err());
                let nonce = [9; 32];
                SHSP::Challenge { nonce: nonce.into() }.send(&mut tx).await.unwrap();
                let CHandshakePacket::Proof { sig } =
                    CHandshakePacket::unpack(&mut rx).await.unwrap()
                else {
                    panic!("expected authentication proof")
                };
                common::crypto::verify_ed25519(
                    &me.verifying_key().to_bytes(),
                    &common::proto::client_rel::client_auth_message(
                        &nonce,
                        &common::quic::client_auth_binding(&conn).unwrap(),
                    ),
                    &sig.0,
                )
                .unwrap();
                SHSP::HandshakeResult(SHSRP::Accept {
                    timestamp: now_ms(),
                    relay_node_id: None,
                    assist: false,
                    turn_port: None,
                })
                .send(&mut tx)
                .await
                .unwrap();
                tx.finish().unwrap();
            })
            .await
            .expect("authentication exceeded the relay's deadline");
            conn
        };
        let (connected, peer) = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::join!(&mut connect, server)
        })
        .await
        .unwrap();
        let Ok(handle) = connected else { panic!("primary connection failed after profile pass") };
        let session = scope.core.session().expect("authenticated session must be published");
        assert!(scope.core.profile_update.try_lock().is_ok());
        assert!(session.conn.close_reason().is_none());
        scope.core.cancel.cancel();
        peer.close(0u32.into(), b"test complete");
        session.conn.close(0u32.into(), b"test complete");
        handle.abort();
        let _ = handle.await;
    }
}
