//! Recipient drain: the local `messages` and `queue` keyspaces, then copies fetched from the
//! other homes. Homes drop their copies on a `QueueFetchAck` after the client's `AckDrain`.

use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::dht_p2p::MAX_FETCH_QUEUE_ACK_IDS;
use common::quic::id::NodeId;
use common::trace;
use common::utils::now_ms;
use common::warn;
use quinn::SendStream;
use tokio::sync::oneshot;

use crate::quic::handler::client::AckAuthPayload;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::RemoteDrainState;
use crate::quic::handler::client::events::drain_auth::DrainAuth;
use crate::quic::handler::client::events::forward::dispatch_to_deliver;
use crate::storage::MessageKey;
use crate::storage::queue::Queue;
use crate::storage::queued_dispatch;

/// Bytes one `DrainQueue` ships before stopping; the client re-issues it for the rest.
const DRAIN_MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Ids one `DrainQueue` ships, so the client's `AckDrain` stays well inside one frame.
const DRAIN_MAX_BATCH_IDS: usize = 16 * 1024;

/// Delivered entries stay on disk until an `AckDrain` names them, once the client has stored
/// them durably.
pub(super) async fn handle_drain_queue(ctx: ClientCtxHandle, tx: &mut SendStream) -> Result<()> {
    let recipient_arr: [u8; 32] = *ctx.ipk.as_bytes();

    let (remote_homes, i_am_home) = match ctx.relay.dht.as_ref() {
        Some(dht) => crate::dht::routing::homes(dht, &NodeId::from_bytes(recipient_arr)),
        None => (Vec::new(), true),
    };
    *ctx.pending_remote_drain.lock() = None;

    let mut delivered_keys: Vec<MessageKey> = Vec::new();
    let mut batch = DrainBatch::default();
    let now_ms = now_ms();

    stream_keyspace(
        &ctx.relay.store.messages,
        &recipient_arr,
        now_ms,
        tx,
        &mut batch,
        &mut delivered_keys,
    )
    .await?;

    if i_am_home && let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        let dht_ids = stream_keyspace(
            &dht.store.queue,
            &recipient_arr,
            now_ms,
            tx,
            &mut batch,
            &mut delivered_keys,
        )
        .await?;
        // The other homes may hold copies of these too; seed the `QueueFetchAck` round so they
        // drop them as well.
        if !dht_ids.is_empty() && !remote_homes.is_empty() {
            *ctx.pending_remote_drain.lock() =
                Some(RemoteDrainState { ids: dht_ids, homes: remote_homes.clone() });
        }
    }

    // Fetch from the other homes too: a write needs only a quorum, so a copy may live only there.
    let auth_snapshot: Option<DrainAuth> = ctx.drain_auth.lock().clone();

    let mut remote_msgs: Vec<DispatchP> = Vec::new();
    if !batch.is_full() {
        if let (Some(auth), Some(dht)) = (auth_snapshot, ctx.relay.dht.as_ref()) {
            remote_msgs = crate::dht::queue_drain::fetch_remote_queues(
                dht,
                &recipient_arr,
                &auth,
                &remote_homes,
            )
            .await;
        } else {
            trace!("DRAIN: no drain auth or DHT, serving local only");
        }
    }

    // The ack may name only ids that reached the wire: the client signs only what it received,
    // and the byte budget can cut the stream short. The rest stay queued for the next drain.
    let mut delivered_remote: Vec<[u8; 16]> = Vec::new();
    for dispatch in remote_msgs {
        if batch.is_full() {
            break;
        }
        // Expired: skipped here, and the home drops it on its own sweep.
        if dispatch.is_expired(now_ms) {
            continue;
        }
        let deliver = dispatch_to_deliver(dispatch);
        if !batch.admit(deliver.id.0, deliver.payload.0.len()) {
            continue;
        }
        trace!("DRAIN: sending queued message id={}", hex::encode(deliver.id));
        let id = deliver.id.0;
        SRelayPacket::Deliver(deliver).send(tx).await?;
        delivered_remote.push(id);
    }

    if !delivered_remote.is_empty() {
        let mut pending = ctx.pending_remote_drain.lock();
        let state =
            pending.get_or_insert_with(|| RemoteDrainState { ids: Vec::new(), homes: Vec::new() });
        state.ids.extend(delivered_remote);
        for home in remote_homes {
            if !state.homes.iter().any(|h| h.id == home.id) {
                state.homes.push(home);
            }
        }
    }

    // Replace, not extend: a re-drain re-reads everything still on disk.
    *ctx.pending_drain.lock() = delivered_keys;

    Ok(())
}

/// The home ack is best-effort: a lost one only means duplicate deliveries, which the client
/// dedupes by id.
pub(super) async fn handle_ack_drain(
    ctx: ClientCtxHandle, ids: Vec<[u8; 16]>, tx: &mut SendStream,
) -> Result<()> {
    let stored: std::collections::HashSet<[u8; 16]> = ids.into_iter().collect();
    // A key may come from either keyspace; removing it from the other is a no-op.
    let mut keys = std::mem::take(&mut *ctx.pending_drain.lock());
    keys.retain(|key| stored.contains(&key.id));
    if !keys.is_empty() {
        let stored_keys: Vec<_> = keys.iter().map(|key| key.as_bytes().into()).collect();
        ctx.relay.store.messages.remove_many(&stored_keys)?;
        ctx.relay.store.queue.remove_many(&stored_keys)?;
        trace!("DRAIN: cleared {} acked messages", keys.len());
    }

    let remote_state = ctx.pending_remote_drain.lock().take();
    if let Some(mut state) = remote_state {
        state.ids.retain(|id| stored.contains(id));
        if !state.ids.is_empty()
            && let Err(err) = run_remote_ack_round(&ctx, tx, state).await
        {
            trace!("DRAIN: remote ack-fanout fell through: {err}");
        }
    }

    Ok(())
}

async fn run_remote_ack_round(
    ctx: &ClientCtxHandle, tx: &mut SendStream, state: RemoteDrainState,
) -> Result<()> {
    // Drains on parallel streams can both deliver an id.
    let mut seen = std::collections::HashSet::new();
    let mut ids = state.ids;
    ids.retain(|id| seen.insert(*id));
    // The home verifier rejects a longer list.
    ids.truncate(MAX_FETCH_QUEUE_ACK_IDS);

    // The latest round wins: replacing the parked sender ends an older one.
    let (sender, mut receiver) = oneshot::channel::<AckAuthPayload>();
    *ctx.ack_auth.lock() = Some(sender);

    // The client binds the ack to this relay's id; homes check it against the authenticated
    // peer, which defeats cross-relay replay.
    let suggested_timestamp = now_ms();
    let requester_relay_id = match ctx.relay.dht.as_ref() {
        Some(dht) => dht.node_id,
        None => return Ok(()),
    };
    SRelayPacket::AckAuthRequest {
        requester_relay_id,
        delivered_ids: ids.clone(),
        suggested_timestamp,
    }
    .send(tx)
    .await?;

    let payload = match tokio::time::timeout(Duration::from_secs(5), &mut receiver).await {
        Ok(Ok(p)) => p,
        result => {
            if result.is_err() {
                warn!("DRAIN: AckAuth timeout (5s); skipping QueueFetchAck fan-out");
            } else {
                warn!("DRAIN: AckAuth channel closed before signature arrived");
            }
            // A newer round may have replaced the parked sender. Dropping this round's receiver
            // closes only this round's sender, so only that one is cleared.
            drop(receiver);
            let mut parked = ctx.ack_auth.lock();
            if parked.as_ref().is_some_and(|sender| sender.is_closed()) {
                *parked = None;
            }
            return Ok(());
        },
    };

    if let Some(dht) = ctx.relay.dht.as_ref() {
        crate::dht::queue_drain::ack_remote_queues(
            dht,
            ctx.ipk.as_bytes(),
            ids,
            payload.timestamp,
            payload.sig,
            &state.homes,
        )
        .await;
    }
    Ok(())
}

#[derive(Default)]
struct DrainBatch {
    seen: std::collections::HashSet<[u8; 16]>,
    bytes: usize,
}

impl DrainBatch {
    fn is_full(&self) -> bool {
        self.bytes >= DRAIN_MAX_BATCH_BYTES || self.seen.len() >= DRAIN_MAX_BATCH_IDS
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

/// Page keys before awaiting the network; retire expired rows in bounded storage batches.
async fn stream_keyspace(
    ks: &Queue, recipient: &[u8; 32], now_ms: u64, tx: &mut SendStream, batch: &mut DrainBatch,
    keys: &mut Vec<MessageKey>,
) -> Result<Vec<[u8; 16]>> {
    let mut sent = Vec::new();
    let mut cursor = None;
    while !batch.is_full() {
        let page = ks.key_page(recipient, cursor.take())?;
        if page.is_empty() {
            break;
        }
        cursor = page.last().cloned();
        let mut expired = Vec::new();
        for key_bytes in page {
            if batch.is_full() {
                break;
            }
            let Some(key) = MessageKey::parse(&key_bytes) else {
                warn!("DRAIN: malformed queue key (len={}); skipping", key_bytes.len());
                continue;
            };
            let Ok(Some(value)) = ks.get(&key_bytes) else { continue };
            let Some(dispatch) = queued_dispatch(recipient, &value) else {
                warn!("DRAIN: malformed queue value; skipping");
                continue;
            };
            if dispatch.is_expired(now_ms) {
                expired.push(key_bytes);
                continue;
            }
            keys.push(key);
            if !batch.admit(dispatch.id.0, value.len()) {
                continue;
            }
            trace!("DRAIN: sending queued message id={}", hex::encode(dispatch.id));
            sent.push(dispatch.id.0);
            SRelayPacket::Deliver(dispatch_to_deliver(dispatch)).send(tx).await?;
        }
        let _ = ks.remove_many(&expired);
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drain the client cannot read would wedge it, so a batch stops at its byte budget, sends
    /// each id once across the local queues and the other homes, and saturates instead of
    /// overflowing.
    #[test]
    fn a_drain_batch_admits_each_id_once_and_fills_at_its_byte_budget() {
        let mut batch = DrainBatch::default();
        assert!(batch.admit([1; 16], DRAIN_MAX_BATCH_BYTES - 1));
        assert!(!batch.admit([1; 16], 1), "a second copy of an id is not sent");
        assert!(!batch.is_full());
        assert!(batch.admit([2; 16], 1));
        assert!(batch.is_full());
        assert!(batch.admit([3; 16], usize::MAX));
        assert_eq!(batch.bytes, usize::MAX);
    }
}
