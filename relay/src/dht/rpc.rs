//! Relay-to-relay requests: one bi-stream per request, the dial included in a budget from `config`.

use std::sync::Arc;
use std::time::Duration;

use common::contracts::services;
use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use quinn::Connection;
use tokio::time::timeout;

use super::Dht;
use super::config::FORWARD_TIMEOUT_MS;

/// `None` on any failure or a missed budget. A KeyPackage request first checks the peer's service
/// guarantee on the same connection, so a reconnect cannot inherit old capabilities.
pub(crate) async fn rpc(
    dht: &Arc<Dht>, peer: &NodeDescriptor, req: &DhtRequest, budget_ms: u64,
) -> Option<DhtResponse> {
    timeout(Duration::from_millis(budget_ms), async {
        let conn = super::lookup::connect_to_peer(dht, peer).await.ok()?;
        if let Some((id, version)) = required_service(req) {
            let DhtResponse::ServiceCapabilities { supported } =
                exchange(&conn, DhtRequest::ServiceCapabilities).await?
            else {
                return None;
            };
            if !common::contracts::Support::decode(&supported.0).ok()?.supports(id, version) {
                return None;
            }
        }
        exchange(&conn, req.clone()).await
    })
    .await
    .ok()
    .flatten()
}

/// Every peer at once. A peer that fails or misses the budget is left out.
pub(crate) async fn fan_out(
    dht: &Arc<Dht>, peers: &[NodeDescriptor], req: &DhtRequest, budget_ms: u64,
) -> Vec<(NodeId, DhtResponse)> {
    let mut set = tokio::task::JoinSet::new();
    for peer in peers.iter().cloned() {
        let (dht, req) = (dht.clone(), req.clone());
        set.spawn(async move { Some((peer.id, rpc(&dht, &peer, &req, budget_ms).await?)) });
    }
    let mut replies = Vec::with_capacity(peers.len());
    while let Some(joined) = set.join_next().await {
        replies.extend(joined.ok().flatten());
    }
    replies
}

/// Asks every home of `target`, this relay through its own dispatcher, which applies the same
/// durability barriers. Returns the homes asked and the replies that arrived.
pub(crate) async fn ask_homes(
    dht: &Arc<Dht>, target: &NodeId, req: DhtRequest,
) -> (Vec<NodeId>, Vec<(NodeId, DhtResponse)>) {
    let (peers, is_home) = super::routing::homes(dht, target);
    let mut asked: Vec<NodeId> = peers.iter().map(|peer| peer.id).collect();
    let mut replies = Vec::with_capacity(asked.len() + 1);
    if is_home {
        asked.push(dht.node_id);
        let local = super::handler::handle_dht_request(dht, req.clone(), dht.node_id).await;
        replies.push((dht.node_id, local));
    }
    replies.extend(fan_out(dht, &peers, &req, FORWARD_TIMEOUT_MS).await);
    (asked, replies)
}

fn required_service(req: &DhtRequest) -> Option<(u16, u16)> {
    match req {
        DhtRequest::KeyPackagePublish(_)
        | DhtRequest::KeyPackageRefill(_)
        | DhtRequest::KeyPackageFetch(_) => {
            Some((services::KEY_PACKAGE_CUSTODY, services::KEY_PACKAGE_CUSTODY_VERSION))
        },
        DhtRequest::KeyPackageInventory { .. } => {
            Some((services::KEY_PACKAGE_INVENTORY, services::KEY_PACKAGE_INVENTORY_VERSION))
        },
        _ => None,
    }
}

async fn exchange(conn: &Connection, req: DhtRequest) -> Option<DhtResponse> {
    let bytes = DhtPacket::Request(req).pack().ok()?;
    let (mut send, mut recv) = conn.open_bi().await.ok()?;
    send.write_all(&bytes).await.ok()?;
    send.finish().ok()?;
    match DhtPacket::unpack(&mut recv).await.ok()? {
        DhtPacket::Response(response) => Some(response),
        DhtPacket::Request(_) => None,
    }
}
