use std::num::NonZeroU32;
use std::sync::Arc;

use common::crypto::PublicKey;
use common::debug;
use common::proto::client_rel::CRelayPacket;
use common::proto::pack::Unpacker;
use common::server::accept::quota;
use common::warn;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::InMemoryState;
use governor::state::NotKeyed;
use governor::state::keyed::DefaultKeyedStateStore;
use parking_lot::Mutex;
use quinn::Connection;
use tokio::sync::Semaphore;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::storage::MessageKey;

use crate::quic::handler::Handler;

const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
use crate::quic::handler::client::events::drain_auth::DrainAuth;
use crate::quic::handler::client::events::handle_packet;
use crate::quic::handler::client::handshake::handle_handshake;
use crate::relay::RelayRef;

pub(crate) mod events;
mod handshake;

/// The client's signature over `queue_fetch_ack_signing_input`, forwarded to each home in a
/// `QueueFetchAck`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AckAuthPayload {
    pub sig:       [u8; 64],
    pub timestamp: u64,
}

const SUBSCRIBE_PRESENCE_PER_MIN: u32 = 6;
const SET_PRESENCE_PER_MIN: u32 = 30;
const REGISTER_PUSH_PER_MIN: u32 = 4;
/// Well below the home's `MAX_KP_FETCH_PER_HOUR`, which is keyed on the relay
/// and would otherwise be spent by whichever co-tenant asks first.
const FETCH_KEYPACKAGE_PER_TARGET_PER_HOUR: u32 = 10;
/// Dispatches and activity pings. Homes rate-limit fan-outs per relay, so one spraying client
/// could get this relay's DHT links closed. The burst covers a reconnect flushing a backlog.
const DISPATCH_PER_MIN: u32 = 240;
const DISPATCH_BURST: u32 = 60;

type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;
type TargetLimiter = RateLimiter<[u8; 32], DefaultKeyedStateStore<[u8; 32]>, DefaultClock>;

/// Per-connection quotas for packets that cost signatures, fsyncs or a fan-out.
pub(crate) struct ClientLimits {
    pub subscribe_presence: DirectLimiter,
    pub set_presence:       DirectLimiter,
    pub register_push:      DirectLimiter,
    pub fetch_keypackage:   TargetLimiter,
    pub dispatch:           DirectLimiter,
}

impl ClientLimits {
    fn new() -> Self {
        let per_minute = |n| RateLimiter::direct(quota(n, n));
        Self {
            dispatch:           RateLimiter::direct(quota(DISPATCH_PER_MIN, DISPATCH_BURST)),
            subscribe_presence: per_minute(SUBSCRIBE_PRESENCE_PER_MIN),
            set_presence:       per_minute(SET_PRESENCE_PER_MIN),
            register_push:      per_minute(REGISTER_PUSH_PER_MIN),
            fetch_keypackage:   RateLimiter::keyed(per_hour(
                FETCH_KEYPACKAGE_PER_TARGET_PER_HOUR,
            )),
        }
    }
}

fn per_hour(n: u32) -> Quota {
    let n = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    Quota::per_hour(n).allow_burst(n)
}

pub struct ClientContext {
    pub ipk: PublicKey,
    pub relay: RelayRef,
    pub conn: Connection,

    pub limits: ClientLimits,

    /// Cancelled at process shutdown, not when this connection ends.
    pub cancel: CancellationToken,
    /// Keys the last `DrainQueue` sent, deleted once an `AckDrain` names their id.
    pub pending_drain: Mutex<Vec<MessageKey>>,

    /// The latest verified `DrainAuth`; a newer one replaces it.
    pub drain_auth: Mutex<Option<DrainAuth>>,

    /// Parked by `run_remote_ack_round` and answered by the `AckAuth` arm of `handle_packet`.
    /// One round is pending at a time; a newer one replaces it.
    pub ack_auth: Mutex<Option<oneshot::Sender<AckAuthPayload>>>,

    /// Ids from the last drain that other homes hold. The homes are acked only after `AckDrain`,
    /// the client's proof that it stored the messages durably.
    pub pending_remote_drain: Mutex<Option<RemoteDrainState>>,
}

#[derive(Clone, Debug)]
pub(crate) struct RemoteDrainState {
    pub ids:   Vec<[u8; 16]>,
    /// Cached: the routing table may shift between fetch and ack.
    pub homes: Vec<common::proto::dht_p2p::NodeDescriptor>,
}

pub type ClientCtxHandle = Arc<ClientContext>;

/// Removes `ipk` only if it still maps to `owned`, so a stale cleanup cannot evict the entry of
/// a newer handshake.
pub(crate) fn remove_client_if_same(relay: &RelayRef, ipk: &[u8; 32], owned: &Connection) -> bool {
    let mut clients = relay.clients.write();
    let same = clients
        .get(ipk)
        .map(|c| c.stable_id() == owned.stable_id())
        .unwrap_or(false);
    if same {
        clients.remove(ipk);
    }
    same
}

impl Handler {
    pub async fn handle_client(self, relay: RelayRef, cancel: CancellationToken) {
        let conn = self.conn.clone();
        let addr = self.conn.remote_address();

        debug!("incoming conn from client({addr})");

        // The acceptor's timeout covers only TLS. Without this, a connection that never sends
        // Hello holds its slot for as long as it keeps the idle timer alive.
        let ipk = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handle_handshake(relay.clone(), &conn)).await {
            Ok(Ok(ipk)) => ipk,
            Ok(Err(err)) => {
                warn!("client({addr}) handshake failed: {err}");
                return;
            },
            Err(_) => {
                warn!("client({addr}) handshake timed out");
                conn.close(0u32.into(), b"handshake timeout");
                return;
            },
        };

        let context = Arc::new(ClientContext {
            ipk,
            relay: relay.clone(),
            conn: conn.clone(),
            limits: ClientLimits::new(),
            cancel: cancel.clone(),
            pending_drain: Mutex::new(Vec::new()),
            drain_auth: Mutex::new(None),
            ack_auth: Mutex::new(None),
            pending_remote_drain: Mutex::new(None),
        });

        let limiter = Arc::new(Semaphore::new(16));

        loop {
            let accept = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!("client({addr}) loop cancelled by shutdown");
                    break;
                }
                accept = conn.accept_bi() => accept,
            };
            let (mut send, mut recv) = match accept {
                Ok(s) => s,
                Err(_) => break,
            };

            let permit = match limiter.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    continue;
                },
            };

            let context = context.clone();
            tokio::spawn(async move {
                let _permit = permit;

                while let Ok(packet) = CRelayPacket::unpack(&mut recv).await {
                    if let Err(err) = handle_packet(packet, context.clone(), &mut send).await {
                        warn!("client({addr}) packet handler failed: {err}");
                    }
                }
            });
        }

        if let Some(close_reason) = self.conn.close_reason() {
            debug!("conn client({addr}) closed: {close_reason}");
        }

        let removed = remove_client_if_same(&relay, ipk.as_bytes(), &self.conn);

        // After the eviction above, so this IPK no longer reads as online.
        if removed {
            events::presence::on_disconnect(&relay, &ipk.to_bytes(), &cancel).await;
        }
    }
}
