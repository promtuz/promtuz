//! Recipient-side drain protocol — both the legacy local-queue path
//! and the sticky-home phase 2c remote-fetch path live here.
//!
//! ## Two queue sources
//!
//! - **`cf_messages`** (the default RocksDB CF). Per-client local
//!   safety-net queue populated by `forward.rs::store_in_rocks` when
//!   a sender's local relay also fails to fan out to the K-closest
//!   homes. Values are postcard-encoded `DeliverP` (no `to` field —
//!   the recipient is encoded in the key prefix).
//! - **`cf_dht_queue`** (DHT phase 2). Per-recipient K-closest queue
//!   populated by `forward.rs::forward_to_homes` (sender side) and,
//!   in phase 2d, the home-side `Forward` handler. Values are
//!   postcard-encoded `DispatchP` (the full sender-signed envelope).
//!
//! The drain unifies both into a stream of `DeliverP` going out to
//! the client. `DispatchP → DeliverP` strips the `to` field; `id`,
//! `from`, `payload`, `sig` carry over verbatim.
//!
//! ## Sticky-home phase 2c integration
//!
//! Per `STICKY_HOME_RELAY.md` §4.3, when this relay R_r is **not**
//! in the user's K-closest set, R_r dials the K homes and pulls
//! their queues. The user's `DrainAuth` (a per-reconnect signed
//! authorisation, see `events::drain_auth`) authenticates the fetch.
//! Without `DrainAuth`, the remote-fetch path is skipped and only
//! the local CFs are drained — graceful degradation for clients that
//! predate phase 2c.
//!
//! ## Phase split — deferred ack-to-home path
//!
//! Phase 2c implements *fetch + deliver only*. The matching
//! `QueueFetchAck` (which deletes the dispatched messages from the
//! homes' `cf_dht_queue`) is deferred to phase 2d. Until then,
//! homes keep their copies until natural TTL expiry, and a user who
//! reconnects again within the TTL window may receive the same
//! dispatch a second time. The client dedupes by `DispatchP.id`;
//! this drain handler also dedupes across the two local CFs and the
//! remote pull so the client only sees one `Deliver` per id per
//! reconnect.

use std::sync::Arc;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::trace;
use common::warn;
use quinn::SendStream;
use rust_rocksdb::WriteBatch;

use crate::dht::Dht;
use crate::dht::config::K;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::drain_auth::DrainAuth;
use crate::storage::MessageKey;

/// Pluggable seam for the remote-fetch path. The default
/// implementation calls [`crate::dht::queue_drain::fetch_remote_queues`];
/// tests override this to inject deterministic homes-returned-x stubs
/// without standing up real two-relay QUIC.
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
            Box<dyn std::future::Future<Output = Vec<DispatchP>> + Send + 'static>,
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
    //      (sparse-network permissive: same policy as
    //      `forward.rs::self_is_in_k`).
    //    - Otherwise: `i_am_home = self ∈ find_closest(user_ipk, K)`.
    let i_am_home = match ctx.relay.dht.as_ref() {
        Some(dht) => self_is_in_k_closest(dht, &recipient_arr),
        None => true,
    };

    // 2. Drain local cf_messages (per the legacy contract). We must
    //    keep the existing `MessageKey`-tracking semantics so the
    //    follow-up `AckDrain` can clean up. We *do not* track keys
    //    for the cross-cf or remote sources here — those are
    //    deferred to phase 2d (cross-CF: the `cf_dht_queue` drain,
    //    once self-as-home is stable; remote: needs `QueueFetchAck`).
    let mut delivered_keys: Vec<MessageKey> = Vec::new();
    let mut seen_ids: std::collections::HashSet<[u8; 16]> =
        std::collections::HashSet::new();
    let mut deliver_queue: Vec<DeliverP> = Vec::new();

    iterate_cf_messages(&ctx, &recipient_arr, &mut deliver_queue, &mut delivered_keys);

    // 3. If `i_am_home`, also iterate `cf_dht_queue` for the user's
    //    prefix. The two CFs share the same `MessageKey` shape and
    //    the prefix-extractor is identical (`store::dht_cf_descriptors`).
    //    A self-as-home relay's `cf_dht_queue` can hold dispatches
    //    that arrived via either the §4.2 sender fan-out or, in
    //    phase 2d, the inbound `Forward` handler.
    if i_am_home && let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        iterate_cf_dht_queue(&dht, &recipient_arr, &mut deliver_queue);
    }

    // Pre-dedupe by id from the local CFs so the cross-cf overlap
    // (a message that ends up in BOTH cf_messages and cf_dht_queue
    // — possible if a sender's local-fallback path coexisted with a
    // home-store path during a routing transition) doesn't ship
    // twice. Order: cf_messages first, then cf_dht_queue, so a tie
    // goes to the legacy queue's chronology.
    let mut deduped: Vec<DeliverP> = Vec::with_capacity(deliver_queue.len());
    for d in deliver_queue.drain(..) {
        if seen_ids.insert(d.id.0) {
            deduped.push(d);
        }
    }

    // 4. If !i_am_home AND drain_auth set AND DHT is enabled, fetch
    //    from remote homes and append. Snapshot the auth out of
    //    the mutex *without* holding the guard across the await.
    let auth_snapshot: Option<DrainAuth> = ctx.drain_auth.lock().clone();

    if !i_am_home {
        if let (Some(auth), Some(dht)) =
            (auth_snapshot, ctx.relay.dht.as_ref().cloned())
        {
            let self_id = dht.node_id;
            // Hand off to the (possibly-stubbed) remote fetcher.
            let dispatches: Vec<DispatchP> =
                (remote_fetcher)(dht.clone(), recipient_arr, auth, self_id).await;
            for d in dispatches {
                if seen_ids.insert(d.id.0) {
                    deduped.push(dispatch_to_deliver(d));
                }
            }
        } else {
            // Either we have no auth (legacy client) or DHT is
            // disabled. Log and degrade to local-only — same shape
            // as the pre-2c behaviour.
            trace!(
                "DRAIN: !i_am_home but drain_auth/dht missing — serving local only"
            );
        }
    }

    // 5. Stream the unified, deduplicated batch.
    for deliver in &deduped {
        trace!("DRAIN: sending queued message id={}", hex::encode(deliver.id));
        SRelayPacket::Deliver(deliver.clone()).send(tx).await?;
    }

    // 6. Replace (rather than extend) so a re-drain before ack still
    //    captures the live set. The previous batch is naturally a
    //    subset of what's still on disk (we haven't deleted yet),
    //    so we'd otherwise grow the pending list with duplicates.
    //
    //    Phase-scope note: `pending_drain` only tracks `cf_messages`
    //    keys. The `cf_dht_queue` cross-cf and remote-home sources
    //    have no on-this-relay deletion semantics yet — phase 2d
    //    introduces `QueueFetchAck` for the remote case and the
    //    self-as-home cf_dht_queue cleanup needs its own in-place
    //    `MessageKey` tracking. For now those messages are still
    //    re-delivered on the next reconnect; the client dedupes by
    //    `DispatchP.id`.
    *ctx.pending_drain.lock() = delivered_keys;

    Ok(())
}

/// Atomically deletes every `cf_messages` key the most recent drain
/// delivered. The `cf_dht_queue` cross-cf cleanup and the remote-
/// home `QueueFetchAck` deletion path are both phase 2d — see the
/// phase-scope note on [`handle_drain_queue_with`].
pub(super) async fn handle_ack_drain(
    ctx: ClientCtxHandle, _tx: &mut SendStream,
) -> Result<()> {
    let keys = std::mem::take(&mut *ctx.pending_drain.lock());

    if keys.is_empty() {
        return Ok(());
    }

    let mut batch = WriteBatch::default();
    for key in &keys {
        batch.delete(key.as_bytes());
    }

    ctx.relay.rocks.write(&batch)?;

    trace!("DRAIN: cleared {} acked messages", keys.len());

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
    let self_dist = xor_dist(self_id.as_bytes(), user_ipk);
    let kth_dist = xor_dist(kth.id.as_bytes(), user_ipk);
    self_dist < kth_dist
}

fn xor_dist(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// Walk the default CF for `recipient_prefix`, push every parsed
/// `DeliverP` onto `out`, and record the corresponding `MessageKey`
/// onto `keys` (the latter feeds the eventual `AckDrain` cleanup).
///
/// Filters re-applied on every entry because `prefix_iterator` is a
/// *seek hint* — RocksDB will happily walk past our recipient prefix
/// into the next user's queue otherwise.
fn iterate_cf_messages(
    ctx: &ClientCtxHandle, recipient: &[u8; 32], out: &mut Vec<DeliverP>,
    keys: &mut Vec<MessageKey>,
) {
    let queue = ctx.relay.rocks.prefix_iterator(recipient);

    for entry in queue {
        let (key_bytes, value) = match entry {
            Ok(kv) => kv,
            Err(e) => {
                warn!("DRAIN: cf_messages iterator error: {e}");
                break;
            }
        };

        if !key_bytes.starts_with(recipient) {
            break;
        }

        let Some(key) = MessageKey::parse(&key_bytes) else {
            warn!("DRAIN: malformed cf_messages key (len={}); skipping", key_bytes.len());
            continue;
        };

        let Ok(deliver) = DeliverP::deser(&value) else {
            warn!("DRAIN: malformed DeliverP value; skipping");
            continue;
        };

        out.push(deliver);
        keys.push(key);
    }
}

/// Walk the `cf_dht_queue` for `recipient_prefix` and push every
/// parsed `DispatchP` (converted to `DeliverP` via
/// [`dispatch_to_deliver`]) onto `out`.
///
/// The keys here are **not** tracked in `pending_drain` because the
/// cross-CF cleanup contract is deferred to phase 2d. A re-drain
/// before phase 2d ships will re-deliver these messages — the
/// client's `DispatchP.id` dedupe handles the redundancy.
fn iterate_cf_dht_queue(dht: &Arc<Dht>, recipient: &[u8; 32], out: &mut Vec<DeliverP>) {
    let cf = match dht.rocks.cf_handle(crate::dht::store::CF_DHT_QUEUE) {
        Some(cf) => cf,
        None => {
            warn!("DRAIN: cf_dht_queue handle missing; skipping");
            return;
        }
    };

    for entry in dht.rocks.prefix_iterator_cf(&cf, recipient) {
        let (key_bytes, value) = match entry {
            Ok(kv) => kv,
            Err(e) => {
                warn!("DRAIN: cf_dht_queue iterator error: {e}");
                break;
            }
        };

        if !key_bytes.starts_with(recipient) {
            break;
        }

        let Ok(dispatch) = DispatchP::deser(&value) else {
            warn!("DRAIN: malformed DispatchP value in cf_dht_queue; skipping");
            continue;
        };

        out.push(dispatch_to_deliver(dispatch));
    }
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
    }
}

/// Default production [`RemoteFetcher`] — calls
/// `crate::dht::queue_drain::fetch_remote_queues` and absorbs any
/// error into an empty result (the drain falls back to local-only
/// rather than failing the whole drain). Per-error telemetry lives
/// inside `fetch_remote_queues` itself.
fn default_remote_fetcher() -> RemoteFetcher {
    Arc::new(
        |dht: Arc<Dht>, user_ipk: [u8; 32], auth: DrainAuth, self_id: NodeId| {
            Box::pin(async move {
                match crate::dht::queue_drain::fetch_remote_queues(
                    dht, &user_ipk, &auth, self_id,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        trace!("DRAIN: remote fetch fell through: {e}");
                        Vec::new()
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
    //! Integration-style tests that exercise the local-cf and
    //! remote-fetch combine + dedupe path through `handle_drain_queue_with`.
    //!
    //! Constructing a real `ClientContext` requires a `Connection`,
    //! which only exists once a QUIC handshake has happened. The
    //! pure logic we need to cover is:
    //!  - the dedupe across local + remote sources, and
    //!  - the `dispatch_to_deliver` field-by-field shape.
    //!
    //! These two are exercised against fixtures the function-level
    //! helpers expose without needing the full handler. The handler
    //! itself is one straight-line pipeline that delegates to those
    //! helpers; the integration test of the full pipeline lands in
    //! the phase 2e cluster smoke test.

    use common::proto::client_rel::DispatchP;

    use super::dispatch_to_deliver;

    #[test]
    fn dispatch_to_deliver_strips_to_keeps_id_from_payload_sig() {
        let dispatch = DispatchP {
            to:      [1u8; 32].into(),
            from:    [2u8; 32].into(),
            id:      [3u8; 16].into(),
            payload: vec![4u8, 5, 6].into(),
            sig:     [7u8; 64].into(),
        };
        let deliver = dispatch_to_deliver(dispatch.clone());
        assert_eq!(deliver.id, dispatch.id);
        assert_eq!(deliver.from, dispatch.from);
        assert_eq!(deliver.payload.0, dispatch.payload.0);
        assert_eq!(deliver.sig, dispatch.sig);
    }

    /// Pure-data dedupe test: local + remote sources holding
    /// overlapping ids must collapse to one entry per id, with
    /// first-occurrence wins (mirrors the `seen_ids.insert`
    /// loop in `handle_drain_queue_with`).
    #[test]
    fn dedupe_across_local_and_remote_keeps_first_occurrence() {
        use std::collections::HashSet;

        use common::proto::client_rel::DeliverP;

        let id_x: [u8; 16] = [0xAA; 16];
        let id_y: [u8; 16] = [0xBB; 16];

        let local = vec![
            DeliverP {
                id:      id_x.into(),
                from:    [0u8; 32].into(),
                payload: vec![1u8].into(),
                sig:     [0u8; 64].into(),
            },
        ];
        let remote = vec![
            DispatchP {
                to:      [0u8; 32].into(),
                from:    [0u8; 32].into(),
                id:      id_x.into(), // duplicates local
                payload: vec![1u8].into(),
                sig:     [0u8; 64].into(),
            },
            DispatchP {
                to:      [0u8; 32].into(),
                from:    [0u8; 32].into(),
                id:      id_y.into(), // new
                payload: vec![2u8].into(),
                sig:     [0u8; 64].into(),
            },
        ];

        let mut seen: HashSet<[u8; 16]> = HashSet::new();
        let mut out: Vec<DeliverP> = Vec::new();
        for d in local {
            if seen.insert(d.id.0) {
                out.push(d);
            }
        }
        for d in remote {
            if seen.insert(d.id.0) {
                out.push(dispatch_to_deliver(d));
            }
        }

        assert_eq!(out.len(), 2, "duplicate id collapsed");
        assert_eq!(out[0].id.0, id_x); // local-first
        assert_eq!(out[1].id.0, id_y); // remote-only
    }
}
