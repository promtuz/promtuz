//! The originating side of the Welcome RPCs. Fetch and ack signatures name this relay as the
//! requester, which every home checks against the connection.

use std::collections::HashSet;
use std::sync::Arc;

use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::mls_wire::WelcomeAckReq;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomeFetchOutcome;
use common::proto::mls_wire::WelcomeFetchReq;
use common::proto::mls_wire::WelcomeFetchResp;
use common::proto::mls_wire::WelcomePublishOutcome;
use common::proto::mls_wire::WelcomePublishReq;
use common::proto::mls_wire::WelcomePublishResp;
use common::quic::id::NodeId;

use super::welcome::stash_prefix;
use crate::dht::Dht;
use crate::dht::config::write_quorum;
use crate::dht::rpc::ask_homes;

/// Whether the selected homes met the write quorum.
pub(crate) async fn originate_welcome_publish(
    dht: &Arc<Dht>, envelope: WelcomeEnvelopeP, timestamp: u64,
) -> bool {
    let target = NodeId::from_bytes(stash_prefix(&envelope.recipient_ipk.0));
    let req = DhtRequest::WelcomePublish(WelcomePublishReq { envelope, timestamp });
    let (asked, replies) = ask_homes(dht, &target, req).await;
    let stored = replies
        .iter()
        .filter(|(_, reply)| {
            matches!(
                reply,
                DhtResponse::WelcomePublish(WelcomePublishResp {
                    outcome: WelcomePublishOutcome::Stored
                })
            )
        })
        .count();
    stored >= write_quorum(asked.len())
}

/// Merges the entries of every reachable home, deduped by `(group_id, kp_ref_used)`.
pub(crate) async fn originate_welcome_fetch(
    dht: &Arc<Dht>, user_ipk: [u8; 32], timestamp: u64, sig: [u8; 64],
) -> Vec<WelcomeEntry> {
    let req = DhtRequest::WelcomeFetch(WelcomeFetchReq {
        user_ipk: user_ipk.into(),
        requester_relay_id: dht.node_id,
        timestamp,
        user_sig: sig.into(),
    });
    let (_, replies) = ask_homes(dht, &NodeId::from_bytes(stash_prefix(&user_ipk)), req).await;

    let mut seen: HashSet<([u8; 32], [u8; 32])> = HashSet::new();
    let mut merged: Vec<WelcomeEntry> = Vec::new();
    for (_, reply) in replies {
        let DhtResponse::WelcomeFetch(WelcomeFetchResp { outcome: WelcomeFetchOutcome::Found(found) }) =
            reply
        else {
            continue;
        };
        for entry in found.welcomes {
            if seen.insert((entry.envelope.group_id.0, entry.envelope.kp_ref_used.0)) {
                merged.push(entry);
            }
        }
    }
    merged
}

/// Best effort with no quorum: a home that misses the ack returns the welcome on the next fetch.
pub(crate) async fn originate_welcome_ack(
    dht: &Arc<Dht>, user_ipk: [u8; 32], welcome_ids: Vec<[u8; 8]>, timestamp: u64, sig: [u8; 64],
) {
    let req = DhtRequest::WelcomeAck(WelcomeAckReq {
        user_ipk: user_ipk.into(),
        requester_relay_id: dht.node_id,
        welcome_ids: welcome_ids.into_iter().map(Into::into).collect(),
        timestamp,
        user_sig: sig.into(),
    });
    ask_homes(dht, &NodeId::from_bytes(stash_prefix(&user_ipk)), req).await;
}
