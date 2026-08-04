use std::num::NonZeroU32;
use std::sync::Arc;

use common::crypto::PublicKey;
use common::debug;
use common::proto::client_rel::CRelayPacket;
use common::proto::pack::Unpacker;
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
use crate::quic::handler::client::events::drain_auth::DrainAuth;
use crate::quic::handler::client::events::handle_packet;
use crate::quic::handler::client::handshake::handle_handshake;
use crate::relay::RelayRef;

pub(crate) mod events;
mod handshake;

/// Buffered client `AckAuth { sig, timestamp }` payload, sent in
/// response to a relay-issued `SRelayPacket::AckAuthRequest`. The
/// signature covers
/// [`common::proto::dht_p2p::queue_fetch_ack_signing_input`] over
/// `(self_ipk, delivered_ids, timestamp)`; the relay then fans the
/// pair out as `QueueFetchAck` to all K homes.
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

type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;
type TargetLimiter = RateLimiter<[u8; 32], DefaultKeyedStateStore<[u8; 32]>, DefaultClock>;

/// Per-connection admission quotas for the client packets whose handling costs
/// the relay signatures, fsyncs or an outbound fan-out.
pub(crate) struct ClientLimits {
    pub subscribe_presence: DirectLimiter,
    pub set_presence:       DirectLimiter,
    pub register_push:      DirectLimiter,
    pub fetch_keypackage:   TargetLimiter,
}

impl ClientLimits {
    fn new() -> Self {
        Self {
            subscribe_presence: RateLimiter::direct(per_minute(SUBSCRIBE_PRESENCE_PER_MIN)),
            set_presence:       RateLimiter::direct(per_minute(SET_PRESENCE_PER_MIN)),
            register_push:      RateLimiter::direct(per_minute(REGISTER_PUSH_PER_MIN)),
            fetch_keypackage:   RateLimiter::keyed(per_hour(
                FETCH_KEYPACKAGE_PER_TARGET_PER_HOUR,
            )),
        }
    }
}

fn per_minute(n: u32) -> Quota {
    let n = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    Quota::per_minute(n).allow_burst(n)
}

fn per_hour(n: u32) -> Quota {
    let n = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    Quota::per_hour(n).allow_burst(n)
}

/// Context for client connection
pub struct ClientContext {
    pub ipk: PublicKey,
    pub relay: RelayRef,
    pub conn: Connection,

    pub limits: ClientLimits,

    /// Cancelled when the process shuts down. Detached fan-out tasks started
    /// from a packet handler select on it so they cannot outlive the runtime.
    pub cancel: CancellationToken,
    /// Keys delivered in the most recent `DrainQueue` whose `AckDrain` we are
    /// still waiting for. Cleared *only* on `AckDrain` so that a re-drain
    /// before the ack lands re-sends the same set rather than dropping it.
    pub pending_drain: Mutex<Vec<MessageKey>>,

    /// Sticky-home — the most recent user-signed `DrainAuth` the
    /// client supplied, or `None` if the client never sent one (legacy
    /// libcore that doesn't supply one).
    ///
    /// Set by `events::drain_auth::handle_drain_auth` after verifying the
    /// transcript signature plus the ±60s freshness window. Read by
    /// `events::drain::handle_drain_queue` when this relay is *not* in
    /// the user's K-closest set and therefore must dial the home relays
    /// to fetch the queue.
    ///
    /// Replace-on-set semantics: a second valid `DrainAuth` overwrites
    /// the first. Because the transcript carries a freshness timestamp,
    /// keeping a stale auth would only force the next remote-fetch
    /// round-trip to fail with `StaleTimestamp` at the home — letting
    /// the client refresh is cheaper.
    pub drain_auth: Mutex<Option<DrainAuth>>,

    /// Sticky-home — pending `AckAuth` request channel.
    ///
    /// When the recipient drain handler issues a
    /// `SRelayPacket::AckAuthRequest` to ask libcore for a per-batch
    /// user signature on the union of `delivered_ids`, it parks a
    /// `oneshot::Sender<AckAuthPayload>` here. The
    /// `CRelayPacket::AckAuth` handler in
    /// `events::ack_auth::handle_ack_auth` takes the sender out, sends
    /// the payload, and lets the awaiter wake up.
    ///
    /// **Single-pending invariant**: only one `AckAuth` round-trip is
    /// in flight per `ClientContext` at any time. A client that
    /// reconnects mid-flight will lose the pending channel (the
    /// `oneshot::Sender` is dropped, the awaiter sees a `RecvError`
    /// and falls back to skipping the home-fanout — the queues
    /// linger until natural TTL expiry, same fallback as on timeout).
    ///
    /// **Lock contract**: `parking_lot::Mutex`; never held across an
    /// `await` (project-wide rule). All callers `.take()` the sender
    /// out of the guard before any I/O.
    pub ack_auth: Mutex<Option<oneshot::Sender<AckAuthPayload>>>,

    /// Sticky-home — pending remote-drain bookkeeping for the
    /// post-`AckDrain` `QueueFetchAck` fan-out.
    ///
    /// Set by the recipient drain handler after a successful
    /// remote-fetch round (when this relay is *not* in the user's
    /// K-closest set, so the queues live at remote homes). Read by
    /// `handle_ack_drain` after the client has durably stored the
    /// drained dispatches; `handle_ack_drain` then issues a
    /// `SRelayPacket::AckAuthRequest`, awaits the `CRelayPacket::AckAuth`,
    /// and fans out a `QueueFetchAck` to each home.
    ///
    /// **Why we buffer rather than sign-then-fan-out inline**: the
    /// transcript signs `(user_ipk, delivered_ids, timestamp)` —
    /// `delivered_ids` is only known after fetching, and the user's
    /// signature on the drain-confirmation must come *after* the
    /// client has actually durably stored the messages (the `AckDrain`
    /// is the durability proof). Splitting the signing from the
    /// fetching mirrors the existing `DrainAuth` / `AckDrain` split.
    pub pending_remote_drain: Mutex<Option<RemoteDrainState>>,
}

/// Buffered "messages just drained from remote homes" state. Lives on
/// `ClientContext.pending_remote_drain` between the `DrainQueue` and
/// the `AckDrain` so the ack-handler can compute the `delivered_ids`
/// union and fan a `QueueFetchAck` out to each home.
#[derive(Clone, Debug)]
pub(crate) struct RemoteDrainState {
    /// User IPK whose queue was drained — same value as `ClientContext.ipk`,
    /// duplicated here so the ack handler doesn't need to round-trip
    /// through `PublicKey::as_bytes`. (Wire shape uses `[u8; 32]`.)
    pub user_ipk: [u8; 32],
    /// Per-home delivered-id map. Each entry's `Vec<[u8; 16]>` is the
    /// list of dispatch ids that home actually returned during the
    /// fetch round. Used to compute the union the ack signs over.
    pub per_home: std::collections::HashMap<common::quic::id::NodeId, Vec<[u8; 16]>>,
    /// The K-closest descriptors at fetch time — re-used as the fan-
    /// out targets for `QueueFetchAck`. Cached because the routing
    /// table may have shifted between fetch and ack.
    pub homes: Vec<common::proto::dht_p2p::NodeDescriptor>,
}

pub type ClientCtxHandle = Arc<ClientContext>;

/// Remove the entry for `ipk` only if its `Connection` is the same one we
/// own (compared by `stable_id()`). This prevents a stale cleanup task from
/// wiping a freshly-registered re-handshake's entry.
///
/// `Connection` is internally an `Arc`, so cloning is cheap, but it does
/// not expose its inner pointer for `Arc::ptr_eq`. `stable_id()` returns a
/// per-connection unique id that survives clones — equivalent guarantee.
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

        let ipk = match handle_handshake(relay.clone(), &conn).await {
            Ok(ipk) => ipk,
            Err(err) => {
                warn!("client({addr}) handshake failed: {err}");
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

        // only 16 concurrent streams can run at once per connection
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
                    // Optional: reject stream politely
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

        // Deregister client on disconnect — but only if the entry still
        // points at *our* connection. A re-handshake for the same IPK that
        // raced past our `accept_bi` failure would have already replaced
        // the entry; in that case we must leave it alone.
        let removed = remove_client_if_same(&relay, ipk.as_bytes(), &self.conn);

        // Presence: record last-seen and notify mutual online contacts. Runs
        // after the eviction above so this IPK no longer reads as online.
        if removed {
            events::presence::on_disconnect(&relay, &ipk.to_bytes(), &cancel).await;
        }
    }
}
