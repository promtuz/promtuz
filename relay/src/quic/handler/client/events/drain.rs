//! Recipient-side drain protocol — both the legacy local-queue path
//! and the sticky-home remote-fetch path live here.
//!
//! ## Two queue sources
//!
//! - **`cf_messages`** (the default fjall CF). Per-client local
//!   safety-net queue populated by `forward.rs::store_in_rocks` when
//!   a sender's local relay also fails to fan out to the K-closest
//!   homes. Values are postcard-encoded `DeliverP` (no `to` field —
//!   the recipient is encoded in the key prefix).
//! - **`cf_dht_queue`** (the DHT queue CF). Per-recipient K-closest
//!   queue populated by `forward.rs::forward_to_homes` (sender side)
//!   and the home-side `Forward` handler. Values are postcard-encoded
//!   `DispatchP` (the full sender-signed envelope).
//!
//! The drain unifies both into a stream of `DeliverP` going out to
//! the client. `DispatchP → DeliverP` strips the `to` field; `id`,
//! `from`, `payload`, `sig` carry over verbatim.
//!
//! ## Sticky-home remote-fetch integration
//!
//! When this relay R_r is **not** in the user's K-closest set, R_r
//! dials the K homes and pulls their queues. The user's `DrainAuth`
//! (a per-reconnect signed
//! authorisation, see `events::drain_auth`) authenticates the fetch.
//! Without `DrainAuth`, the remote-fetch path is skipped and only
//! the local CFs are drained — graceful degradation for clients that
//! don't supply one.
//!
//! ## Ack-to-home path
//!
//! The remote-fetch path delivers messages, and the matching
//! `QueueFetchAck` (which deletes the dispatched messages from the
//! homes' `cf_dht_queue`) runs afterwards. Should the ack not land,
//! homes keep their copies until natural TTL expiry, and a user who
//! reconnects again within the TTL window may receive the same
//! dispatch a second time. The client dedupes by `DispatchP.id`;
//! this drain handler also dedupes across the two local CFs and the
//! remote pull so the client only sees one `Deliver` per id per
//! reconnect.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::dht_p2p::MAX_FETCH_QUEUE_ACK_IDS;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;
use common::trace;
use common::warn;
use fjall::Keyspace;
use quinn::SendStream;
use tokio::sync::oneshot;

use crate::dht::Dht;
use crate::dht::config::K;
use crate::quic::handler::client::AckAuthPayload;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::RemoteDrainState;
use crate::quic::handler::client::events::drain_auth::DrainAuth;
use crate::storage::MessageKey;
use crate::util::systime;

/// Serialized payload a single `DrainQueue` ships before it stops and lets the
/// client re-issue. Keeps peak drain memory to one message, not one queue.
const DRAIN_MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Extended fetch result that carries the per-home `delivered_ids`
/// map and the home descriptors so the post-`AckDrain` flow can issue
/// `QueueFetchAck` to each home.
#[derive(Clone, Debug, Default)]
pub(crate) struct RemoteFetchResult {
    pub messages: Vec<DispatchP>,
    pub per_home: std::collections::HashMap<NodeId, Vec<[u8; 16]>>,
    pub homes:    Vec<NodeDescriptor>,
}

/// Pluggable seam for the remote-fetch path. The default
/// implementation calls [`crate::dht::queue_drain::fetch_remote_queues_with_homes`];
/// tests override this to inject deterministic homes-returned-x stubs
/// without standing up real two-relay QUIC.
///
/// The result type is [`RemoteFetchResult`] (carries per-home
/// metadata for the `QueueFetchAck` round). Tests that don't care
/// about the ack-fanout half can return a `RemoteFetchResult` with
/// empty `per_home`/`homes`.
///
/// `Send + Sync` because the closure stores in
/// `static`-equivalent state in a relay's `Arc<Dht>`-powered fan-out
/// path.
pub type RemoteFetcher = std::sync::Arc<
    dyn Fn(
            Arc<Dht>,
            [u8; 32],
            DrainAuth,
            NodeId,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = RemoteFetchResult> + Send + 'static>,
        > + Send
        + Sync,
>;

/// Sends all pending messages to the user. The queue is *not* cleared
/// yet — the client must follow up with `AckDrain` (handled by
/// [`handle_ack_drain`]) once it has durably stored everything.
///
/// If the client triggers another `DrainQueue` before acking, we re-
/// send the previously-tracked set plus any newly arrived messages.
/// We do not reset the tracked-key list until the ack arrives.
pub(super) async fn handle_drain_queue(
    ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    handle_drain_queue_with(ctx, tx, default_remote_fetcher()).await
}

/// Inner drain entry point that takes a (test-injectable)
/// [`RemoteFetcher`]. The production wrapper [`handle_drain_queue`]
/// passes [`default_remote_fetcher`].
pub(crate) async fn handle_drain_queue_with(
    ctx: ClientCtxHandle, tx: &mut SendStream, remote_fetcher: RemoteFetcher,
) -> Result<()> {
    let recipient_arr: [u8; 32] = *ctx.ipk.as_bytes();

    // 1. Compute `i_am_home` for this user. Branches:
    //    - DHT disabled → `i_am_home = true` (degenerate but
    //      correct: the local cf_messages drain is exactly what a
    //      pre-DHT relay does).
    //    - Routing table holds < K peers → `i_am_home = true`
    //      (sparse-network permissive: same policy as `forward.rs::self_is_in_k`).
    //    - Otherwise: `i_am_home = self ∈ find_closest(user_ipk, K)`.
    let i_am_home = match ctx.relay.dht.as_ref() {
        Some(dht) => self_is_in_k_closest(dht, &recipient_arr),
        None => true,
    };

    // 2. Stream local cf_messages straight to the wire. `MessageKey`s of
    //    everything read (from BOTH local keyspaces) are tracked in
    //    `delivered_keys` so the follow-up `AckDrain` deletes them. The
    //    remote-home source is GC'd separately via `QueueFetchAck` after
    //    the ack lands.
    let mut delivered_keys: Vec<MessageKey> = Vec::new();
    let mut batch = DrainBatch::default();

    stream_keyspace(
        &ctx.relay.store.messages,
        &recipient_arr,
        decode_deliver,
        tx,
        &mut batch,
        &mut delivered_keys,
    )
    .await?;

    // 3. If `i_am_home`, also stream the `dht_queue` keyspace for the
    //    user's prefix. Both keyspaces share the same `MessageKey` shape.
    //    A self-as-home relay's `dht_queue` can hold dispatches that
    //    arrived via either the sender fan-out or the inbound `Forward`
    //    handler.
    if i_am_home && let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        let dht_ids = stream_keyspace(
            &dht.store.queue,
            &recipient_arr,
            decode_dispatch,
            tx,
            &mut batch,
            &mut delivered_keys,
        )
        .await?;
        // The sender fan-out stored these same dispatches at ALL K homes. We
        // GC our own copy on AckDrain, but the other K-1 keep theirs and
        // redeliver them on every reconnect (now client-deduped, but wasted
        // bandwidth). Tell them to GC too — reuse the QueueFetchAck round by
        // seeding `pending_remote_drain` with the other homes + these ids.
        if !dht_ids.is_empty() {
            let self_id = dht.node_id;
            let others: Vec<NodeDescriptor> = {
                let target = NodeId::from_bytes(recipient_arr);
                let routing = dht.routing.read();
                routing.find_closest(&target, K).into_iter().filter(|d| d.id != self_id).collect()
            };
            if !others.is_empty() {
                let per_home = others.iter().map(|h| (h.id, dht_ids.clone())).collect();
                *ctx.pending_remote_drain.lock() = Some(RemoteDrainState {
                    user_ipk: recipient_arr,
                    per_home,
                    homes: others,
                });
            }
        }
    }

    // 4. If !i_am_home AND drain_auth set AND DHT is enabled, fetch
    //    from remote homes. Snapshot the auth out of the mutex
    //    *without* holding the guard across the await.
    let auth_snapshot: Option<DrainAuth> = ctx.drain_auth.lock().clone();

    let mut remote_msgs: Vec<DispatchP> = Vec::new();
    let mut remote_per_home: std::collections::HashMap<NodeId, Vec<[u8; 16]>> =
        std::collections::HashMap::new();
    let mut remote_homes = Vec::new();
    if !i_am_home && !batch.is_full() {
        if let (Some(auth), Some(dht)) =
            (auth_snapshot, ctx.relay.dht.as_ref().cloned())
        {
            let self_id = dht.node_id;
            // Hand off to the (possibly-stubbed) remote fetcher.
            let result: RemoteFetchResult =
                (remote_fetcher)(dht.clone(), recipient_arr, auth, self_id).await;
            remote_msgs = result.messages;
            remote_per_home = result.per_home;
            remote_homes = result.homes;
        } else {
            // Either we have no auth (legacy client) or DHT is
            // disabled. Log and degrade to local-only — same shape
            // as the local-only drain.
            trace!(
                "DRAIN: !i_am_home but drain_auth/dht missing — serving local only"
            );
        }
    }

    // 5. Stream the remote-fetched dispatches. The local keyspaces went out
    //    first, so a message that landed in BOTH — possible when a sender's
    //    local-fallback path coexisted with a home-store path during a
    //    routing transition — ships once, from the local side.
    let mut delivered_remote: std::collections::HashSet<[u8; 16]> =
        std::collections::HashSet::new();
    for dispatch in remote_msgs {
        if batch.is_full() {
            break;
        }
        let deliver = dispatch_to_deliver(dispatch);
        if !batch.admit(deliver.id.0, deliver.payload.0.len()) {
            continue;
        }
        trace!("DRAIN: sending queued message id={}", hex::encode(deliver.id));
        let id = deliver.id.0;
        SRelayPacket::Deliver(deliver).send(tx).await?;
        delivered_remote.insert(id);
    }

    // The ack transcript may only name ids that reached the wire — the client
    // refuses to sign for anything it did not receive, and the byte budget
    // above can cut the stream short. Ids left behind stay queued at their
    // home and come back on the next drain.
    if !remote_homes.is_empty() {
        for ids in remote_per_home.values_mut() {
            ids.retain(|id| delivered_remote.contains(id));
        }
        remote_per_home.retain(|_, ids| !ids.is_empty());
        *ctx.pending_remote_drain.lock() = Some(RemoteDrainState {
            user_ipk: recipient_arr,
            per_home: remote_per_home,
            homes:    remote_homes,
        });
    }

    // 6. Replace (rather than extend) so a re-drain before ack still
    //    captures the live set. The previous batch is naturally a
    //    subset of what's still on disk (we haven't deleted yet),
    //    so we'd otherwise grow the pending list with duplicates.
    //    Holds keys from both local keyspaces; `handle_ack_drain`
    //    removes each key from both (wrong-keyspace remove = no-op).
    *ctx.pending_drain.lock() = delivered_keys;

    Ok(())
}

/// Atomically deletes every `cf_messages` key the most recent drain
/// delivered, and fans a `QueueFetchAck` out to all K homes that
/// contributed to the remote-fetch round so they GC their copies of
/// the now-acknowledged dispatches.
///
/// **Order of operations**:
/// 1. Local `cf_messages` deletion via WriteBatch (durable).
/// 2. If `pending_remote_drain` is set: ask libcore to sign an ack
///    over the union `delivered_ids` (5s timeout via
///    `oneshot::Receiver`), then send `QueueFetchAck` to each home
///    in parallel (3s total wall-clock budget). Best-effort;
///    failures only mean some queues linger at homes until natural
///    TTL expiry.
///
/// **Why best-effort**: the homes' `cf_dht_queue` keys lasting until
/// TTL is the soft fallback. The user-visible drain has already
/// succeeded at this point — the
/// client got its messages and durably stored them. Failing the ack
/// flow would not change that; it would just leak duplicate
/// deliveries on the next reconnect.
pub(super) async fn handle_ack_drain(
    ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // 1. Local GC — `pending_drain` holds keys from both cf_messages
    //    and (self-as-home) cf_dht_queue. Same 56-byte `MessageKey`
    //    shape in both; removing a key from the keyspace it isn't in
    //    is a no-op, and a dispatch double-stored during a routing
    //    transition is correctly purged from both.
    let keys = std::mem::take(&mut *ctx.pending_drain.lock());
    if !keys.is_empty() {
        let mut batch = ctx.relay.store.batch();
        for key in &keys {
            batch.remove(&ctx.relay.store.messages, key.as_bytes());
            batch.remove(&ctx.relay.store.queue, key.as_bytes());
        }
        batch.commit()?;
        trace!("DRAIN: cleared {} acked messages", keys.len());
    }

    // 2. Remote `QueueFetchAck` fan-out.
    let remote_state = ctx.pending_remote_drain.lock().take();
    if let Some(state) = remote_state
        && let Err(err) = run_remote_ack_round(&ctx, tx, state).await {
            trace!("DRAIN: remote ack-fanout fell through: {err}");
        }

    Ok(())
}

/// Orchestrate the post-`AckDrain` remote-ack round:
/// 1. Compute the union of delivered ids across all homes (caps at
///    [`MAX_FETCH_QUEUE_ACK_IDS`] to match the home-side verifier;
///    overflow truncates oldest-first because per-home iteration
///    order already chronological).
/// 2. Park a `oneshot::Sender<AckAuthPayload>` on `ctx.ack_auth`.
/// 3. Send `SRelayPacket::AckAuthRequest` to the client.
/// 4. Await `CRelayPacket::AckAuth` via the oneshot (5s timeout).
/// 5. Fan out `QueueFetchAck` to each home in parallel (3s total
///    via `queue_drain::ack_remote_queues`).
///
/// Best-effort: every failure path returns `Ok(())` from this
/// function so the user-visible `AckDrain` still succeeds. The
/// `Result<()>` shape exists only so the function can use `?` with
/// the QUIC stream operations.
async fn run_remote_ack_round(
    ctx: &ClientCtxHandle, tx: &mut SendStream, state: RemoteDrainState,
) -> Result<()> {
    // 1. Compute the union of delivered ids.
    let mut union_set: std::collections::HashSet<[u8; 16]> =
        std::collections::HashSet::new();
    let mut union: Vec<[u8; 16]> = Vec::new();
    for ids in state.per_home.values() {
        for id in ids {
            if union_set.insert(*id) {
                union.push(*id);
            }
        }
    }
    if union.is_empty() {
        // No homes contributed — nothing to ack. Skip the round trip.
        return Ok(());
    }
    // Defensively cap to the wire-format ceiling. The home-side
    // verifier rejects oversize lists; truncating saves the
    // round-trip. A drain that produces > 64 messages from remote
    // homes is already unusual (a single page from one home returns
    // up to 64), but bounded paging across multiple homes can reach
    // here.
    if union.len() > MAX_FETCH_QUEUE_ACK_IDS {
        union.truncate(MAX_FETCH_QUEUE_ACK_IDS);
    }

    // 2. Park the response receiver. Replace any stale pending sender
    //    — the latest ack round wins.
    let (sender, receiver) = oneshot::channel::<AckAuthPayload>();
    *ctx.ack_auth.lock() = Some(sender);

    // 3. Send the AckAuthRequest to the client. Include
    //    `requester_relay_id` so libcore signs the per-K-home ack
    //    transcript binding to *this* relay's identity. The home
    //    cross-checks `requester_relay_id == authenticated_peer_id`
    //    to defeat cross-relay replay.
    let suggested_timestamp = systime().as_millis() as u64;
    let requester_relay_id = match ctx.relay.dht.as_ref() {
        Some(dht) => dht.node_id,
        // No DHT is the legacy / DHT-disabled deployment; the
        // ack round can't reach a home regardless, so abandon
        // gracefully. The earlier short-circuit on empty `union`
        // catches the common case but a non-DHT relay can still
        // reach this path with a non-empty `union` if the union
        // was carried in from a different drain round.
        None => return Ok(()),
    };
    SRelayPacket::AckAuthRequest {
        requester_relay_id,
        delivered_ids: union.clone(),
        suggested_timestamp,
    }
    .send(tx)
    .await?;

    // 4. Await the client's signed ack with a 5s timeout. On timeout
    //    or channel close, drop the pending sender (best-effort:
    //    homes won't get the ack, queues linger until TTL expiry).
    let payload = match tokio::time::timeout(Duration::from_secs(5), receiver).await {
        Ok(Ok(p)) => p,
        Ok(Err(_)) => {
            // Sender dropped before we received — likely the client
            // disconnected. Clear the pending entry.
            *ctx.ack_auth.lock() = None;
            warn!("DRAIN: AckAuth channel closed before signature arrived");
            return Ok(());
        }
        Err(_) => {
            // Timeout. Clear the pending entry so a future
            // `AckAuthRequest` can install a fresh sender.
            *ctx.ack_auth.lock() = None;
            warn!("DRAIN: AckAuth timeout (5s); skipping QueueFetchAck fan-out");
            return Ok(());
        }
    };

    // 5. Fan out to all homes. Best-effort, bounded to 3s total.
    if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        crate::dht::queue_drain::ack_remote_queues(
            dht,
            &state.user_ipk,
            union,
            payload.timestamp,
            payload.sig,
            state.homes,
        )
        .await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `true` iff `self_id ∈ find_closest(user_ipk, K)` under the same
/// permissive sparse-table policy as `forward.rs::forward_to_homes`.
fn self_is_in_k_closest(dht: &Dht, user_ipk: &[u8; 32]) -> bool {
    let target = NodeId::from_bytes(*user_ipk);
    let descriptors = {
        let routing = dht.routing.read();
        routing.find_closest(&target, K)
    };
    let self_id = dht.node_id;

    if descriptors.len() < K {
        // Sparse table → permissively count self as home (same as
        // `forward_to_homes::self_is_in_k`).
        return true;
    }

    let kth = &descriptors[K - 1];
    let self_dist = xor32(self_id.as_bytes(), user_ipk);
    let kth_dist = xor32(kth.id.as_bytes(), user_ipk);
    self_dist < kth_dist
}

/// Running state of one drain: which ids have already gone out and how many
/// serialized bytes that cost.
#[derive(Default)]
struct DrainBatch {
    seen:  std::collections::HashSet<[u8; 16]>,
    bytes: usize,
}

impl DrainBatch {
    fn is_full(&self) -> bool {
        self.bytes >= DRAIN_MAX_BATCH_BYTES
    }

    /// `false` when `id` already went out this drain. The caller still tracks
    /// the key, so both copies of a double-stored dispatch are GC'd on ack.
    fn admit(&mut self, id: [u8; 16], size: usize) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.bytes = self.bytes.saturating_add(size);
        true
    }
}

fn decode_deliver(value: &[u8]) -> Option<DeliverP> {
    DeliverP::deser(value).ok()
}

fn decode_dispatch(value: &[u8]) -> Option<DeliverP> {
    DispatchP::deser(value).ok().map(dispatch_to_deliver)
}

/// Walk `ks` for `recipient`, sending each decoded entry to the client as it is
/// read and recording its `MessageKey` onto `keys` for the eventual `AckDrain`
/// cleanup. Returns the ids actually sent. Stops once `batch` is full; the
/// untouched remainder stays on disk for the client's next `DrainQueue`.
///
/// Keys are collected up front so no keyspace iterator is held across the
/// `await` that writes to the wire.
async fn stream_keyspace(
    ks: &Keyspace, recipient: &[u8; 32], decode: fn(&[u8]) -> Option<DeliverP>,
    tx: &mut SendStream, batch: &mut DrainBatch, keys: &mut Vec<MessageKey>,
) -> Result<Vec<[u8; 16]>> {
    let mut sent: Vec<[u8; 16]> = Vec::new();
    for key in collect_keys(ks, recipient) {
        if batch.is_full() {
            break;
        }
        let Ok(Some(value)) = ks.get(key.as_bytes()) else { continue };
        let Some(deliver) = decode(&value) else {
            warn!("DRAIN: malformed queue value; skipping");
            continue;
        };
        keys.push(key);
        if !batch.admit(deliver.id.0, value.len()) {
            continue;
        }
        trace!("DRAIN: sending queued message id={}", hex::encode(deliver.id));
        sent.push(deliver.id.0);
        SRelayPacket::Deliver(deliver).send(tx).await?;
    }
    Ok(sent)
}

fn collect_keys(ks: &Keyspace, recipient: &[u8; 32]) -> Vec<MessageKey> {
    let mut keys: Vec<MessageKey> = Vec::new();
    for guard in ks.prefix(recipient) {
        let key_bytes = match guard.key() {
            Ok(k) => k,
            Err(e) => {
                warn!("DRAIN: queue iterator error: {e}");
                break;
            },
        };
        match MessageKey::parse(&key_bytes) {
            Some(key) => keys.push(key),
            None => warn!("DRAIN: malformed queue key (len={}); skipping", key_bytes.len()),
        }
    }
    keys
}

/// `DispatchP → DeliverP` field-by-field. Strips the `to` field
/// (encoded in the key, not the value) and carries `id`, `from`,
/// `payload`, `sig` verbatim.
fn dispatch_to_deliver(d: DispatchP) -> DeliverP {
    DeliverP {
        id:      d.id,
        from:    d.from,
        payload: d.payload,
        sig:     d.sig,
        accepted_at_ms: d.accepted_at_ms,
    }
}

/// Default production [`RemoteFetcher`] — calls
/// [`crate::dht::queue_drain::fetch_remote_queues_with_homes`] and
/// absorbs any error into an empty result (the drain falls back to
/// local-only rather than failing the whole drain). Per-error
/// telemetry lives inside the underlying helper.
///
/// Also computes the K-closest descriptor list (filtered to non-self)
/// and includes it in the result so the `handle_ack_drain` half can
/// fan a `QueueFetchAck` out to those homes without re-walking the
/// routing table.
fn default_remote_fetcher() -> RemoteFetcher {
    Arc::new(
        |dht: Arc<Dht>, user_ipk: [u8; 32], auth: DrainAuth, self_id: NodeId| {
            Box::pin(async move {
                // Snapshot the K-closest descriptors *now* — the same
                // set the underlying fetcher uses internally. Cloning
                // out of the routing-table read lock before any
                // await; never held across.
                let homes: Vec<NodeDescriptor> = {
                    let target_id = NodeId::from_bytes(user_ipk);
                    let routing = dht.routing.read();
                    routing
                        .find_closest(&target_id, K)
                        .into_iter()
                        .filter(|d| d.id != self_id)
                        .collect()
                };
                match crate::dht::queue_drain::fetch_remote_queues_with_homes(
                    dht, &user_ipk, &auth, self_id,
                )
                .await
                {
                    Ok((messages, per_home)) => RemoteFetchResult {
                        messages,
                        per_home,
                        homes,
                    },
                    Err(e) => {
                        trace!("DRAIN: remote fetch fell through: {e}");
                        RemoteFetchResult {
                            messages: Vec::new(),
                            per_home: std::collections::HashMap::new(),
                            homes,
                        }
                    }
                }
            })
        },
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use common::proto::client_rel::DispatchP;

    use super::DRAIN_MAX_BATCH_BYTES;
    use super::DrainBatch;
    use super::dispatch_to_deliver;

    #[test]
    fn dispatch_to_deliver_strips_to_keeps_id_from_payload_sig() {
        let dispatch = DispatchP {
            to:      [1u8; 32].into(),
            from:    [2u8; 32].into(),
            id:      [3u8; 16].into(),
            payload: vec![4u8, 5, 6].into(),
            sig:     [7u8; 64].into(),
            accepted_at_ms: 1,
            wake:    false,
        };
        let deliver = dispatch_to_deliver(dispatch.clone());
        assert_eq!(deliver.id, dispatch.id);
        assert_eq!(deliver.from, dispatch.from);
        assert_eq!(deliver.payload.0, dispatch.payload.0);
        assert_eq!(deliver.sig, dispatch.sig);
        assert_eq!(deliver.accepted_at_ms, dispatch.accepted_at_ms);
    }

    #[test]
    fn drain_batch_admits_an_id_once_across_sources() {
        let mut batch = DrainBatch::default();
        assert!(batch.admit([0xAA; 16], 10));
        assert!(batch.admit([0xBB; 16], 10));
        assert!(!batch.admit([0xAA; 16], 10));
        assert_eq!(batch.bytes, 20);
    }

    #[test]
    fn drain_batch_reports_full_once_the_byte_budget_is_spent() {
        let mut batch = DrainBatch::default();
        assert!(!batch.is_full());
        assert!(batch.admit([1u8; 16], DRAIN_MAX_BATCH_BYTES - 1));
        assert!(!batch.is_full());
        assert!(batch.admit([2u8; 16], 1));
        assert!(batch.is_full());
    }

    #[test]
    fn drain_batch_saturates_rather_than_overflows() {
        let mut batch = DrainBatch::default();
        assert!(batch.admit([1u8; 16], usize::MAX));
        assert!(batch.admit([2u8; 16], usize::MAX));
        assert_eq!(batch.bytes, usize::MAX);
    }
}
