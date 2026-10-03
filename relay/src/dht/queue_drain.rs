//! Draining a user's queue from the homes. `QueueFetch` reads a batch from each home; a later
//! `QueueFetchAck`, signed by the user over the delivered ids, lets each home delete them.

use std::collections::HashSet;
use std::sync::Arc;

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::MAX_FETCH_QUEUE_BATCH;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::QueueFetch;
use common::proto::dht_p2p::QueueFetchAck;
use common::proto::dht_p2p::QueueFetchAckResp;
use common::proto::dht_p2p::QueueFetchResp;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;

use super::Dht;
use super::config::QUEUE_FETCH_TIMEOUT_MS;
use super::rpc::fan_out;
use crate::quic::handler::client::events::drain_auth::DrainAuth;

/// One batch from each of `homes`, which are the remote ones, deduped by id. One batch only: a
/// home's queue changes only on the post-drain ack, so a second fetch would return the same batch.
pub(crate) async fn fetch_remote_queues(
    dht: &Arc<Dht>, user_ipk: &[u8; 32], drain_auth: &DrainAuth, homes: &[NodeDescriptor],
) -> Vec<DispatchP> {
    // One user signature serves every home: the transcript does not name the home.
    let fetch = DhtRequest::QueueFetch(QueueFetch {
        user_ipk:           Bytes(*user_ipk),
        requester_relay_id: dht.node_id,
        timestamp:          drain_auth.timestamp,
        user_sig:           Bytes(drain_auth.sig),
    });

    // The first copy wins, which keeps each home's oldest-first order.
    let mut seen: HashSet<[u8; 16]> = HashSet::new();
    let mut out: Vec<DispatchP> = Vec::new();
    for (_, reply) in fan_out(dht, homes, &fetch, QUEUE_FETCH_TIMEOUT_MS).await {
        let DhtResponse::QueueFetch(QueueFetchResp { messages, .. }) = reply else { continue };
        out.extend(messages.into_iter().filter(|d| seen.insert(d.id.0)));
    }
    out
}

pub(crate) async fn handle_queue_fetch_rpc(
    dht: &Arc<Dht>, req: QueueFetch, authenticated_peer_id: NodeId, now_ms: u64,
) -> QueueFetchResp {
    // The signed requester must be the authenticated peer, so another relay cannot replay a
    // captured fetch to read the queue.
    if req.requester_relay_id != authenticated_peer_id {
        return QueueFetchResp { messages: Vec::new(), exhausted: true };
    }

    if req.verify(now_ms).is_err() {
        return QueueFetchResp { messages: Vec::new(), exhausted: true };
    }

    let user_ipk = req.user_ipk.0;
    if !super::routing::homes(dht, &NodeId::from_bytes(user_ipk)).1 {
        return QueueFetchResp { messages: Vec::new(), exhausted: true };
    }

    let (batch, exhausted) =
        super::store::queue_batch_for_user(dht, &user_ipk, MAX_FETCH_QUEUE_BATCH, now_ms);

    let messages: Vec<DispatchP> = batch.into_iter().map(|(_k, d)| d).collect();
    QueueFetchResp { messages, exhausted }
}

pub(crate) async fn handle_queue_fetch_ack_rpc(
    dht: &Arc<Dht>, req: QueueFetchAck, authenticated_peer_id: NodeId, now_ms: u64,
) -> QueueFetchAckResp {
    // The signed requester must be the authenticated peer, or a relay the user once used could
    // replay a captured ack at the other homes and drop undelivered messages.
    if req.requester_relay_id != authenticated_peer_id {
        return QueueFetchAckResp { ok: false };
    }
    if req.verify(now_ms).is_err() {
        return QueueFetchAckResp { ok: false };
    }
    let user_ipk = req.user_ipk.0;
    let _deleted = super::store::delete_queue_entries(dht, &user_ipk, &req.delivered_ids);
    QueueFetchAckResp { ok: true }
}

/// Best effort: a home that misses the ack redelivers on a later drain, and the client dedupes by
/// id. One signature serves every home, each deleting only the ids it holds.
pub(crate) async fn ack_remote_queues(
    dht: &Arc<Dht>, user_ipk: &[u8; 32], delivered_ids: Vec<[u8; 16]>, timestamp: u64,
    sig: [u8; 64], homes: &[NodeDescriptor],
) {
    let ack = DhtRequest::QueueFetchAck(QueueFetchAck {
        user_ipk: Bytes(*user_ipk),
        requester_relay_id: dht.node_id,
        delivered_ids,
        timestamp,
        user_sig: Bytes(sig),
    });
    fan_out(dht, homes, &ack, QUEUE_FETCH_TIMEOUT_MS).await;
}
