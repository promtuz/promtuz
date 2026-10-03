//! Owner-authorized inventory. This is a read, never a consuming fetch.
use std::sync::Arc;

use common::contracts::services::{
    self,
    key_inventory::{Home, Inventory, Request, Snapshot},
};
use common::crypto::verify_ed25519;
use common::proto::dht_p2p::{DhtRequest, DhtResponse};
use common::proto::mls_wire::key_package_stash_prefix;
use common::quic::id::NodeId;

use crate::dht::Dht;

const _: () =
    assert!(services::key_inventory::MAX_REFERENCES == common::proto::mls_wire::KP_STASH_TARGET);
const _: () = assert!(crate::dht::config::K <= services::key_inventory::MAX_HOMES);

/// The owner authorizes this relay's authenticated peer identity to read only
/// their stash. Capturing the request cannot authorize another relay or owner.
pub(crate) fn authorized(request: &Request, authenticated_delegate: NodeId, now: u64) -> bool {
    if request.delegate != *authenticated_delegate.as_bytes()
        || now.abs_diff(request.timestamp) > services::key_inventory::MAX_SKEW_MS
    {
        return false;
    }
    verify_ed25519(&request.owner, &request.signing_input(), &request.signature).is_ok()
}

async fn read_home(
    dht: &Arc<Dht>, request: &Request, authenticated_delegate: NodeId, now: u64,
) -> Inventory {
    let stash = NodeId::from_bytes(key_package_stash_prefix(&request.owner));
    let snapshot = if authorized(request, authenticated_delegate, now)
        && crate::dht::routing::homes(dht, &stash).1
    {
        match dht.store.key_packages.inventory(&request.owner, now) {
            Ok(references) if dht.store.persist_barrier().wait().await.is_ok() => {
                Some(Snapshot { observed_at_ms: now, references })
            },
            _ => None,
        }
    } else {
        None
    };
    Inventory {
        owner: request.owner,
        request_timestamp: request.timestamp,
        homes: vec![Home { node: *dht.node_id.as_bytes(), snapshot }],
    }
}

pub(crate) async fn handle(
    dht: &Arc<Dht>, bytes: &[u8], authenticated_delegate: NodeId, now: u64,
) -> Vec<u8> {
    let Ok(request) = Request::decode(bytes) else {
        return Vec::new();
    };
    read_home(dht, &request, authenticated_delegate, now).await.encode().unwrap_or_default()
}

/// The exact selected set is retained even when a connection, capability or
/// read fails. Replies from a different home or for another query are rejected.
pub(crate) async fn originate(dht: &Arc<Dht>, request: &Request, now: u64) -> Option<Inventory> {
    if !authorized(request, dht.node_id, now) {
        return None;
    }
    let target = NodeId::from_bytes(key_package_stash_prefix(&request.owner));
    let req = DhtRequest::KeyPackageInventory { request: request.encode().into() };
    let (asked, replies) = crate::dht::rpc::ask_homes(dht, &target, req).await;
    let mut homes: std::collections::BTreeMap<[u8; 32], Option<Snapshot>> =
        asked.iter().map(|node| (*node.as_bytes(), None)).collect();
    for (node, reply) in replies {
        let DhtResponse::KeyPackageInventory { inventory } = reply else { continue };
        let snapshot = Inventory::decode(&inventory.0)
            .ok()
            .filter(|inventory| {
                inventory.matches(request)
                    && inventory.homes.len() == 1
                    && inventory.homes[0].node == *node.as_bytes()
            })
            .and_then(|inventory| inventory.homes.into_iter().next()?.snapshot);
        homes.insert(*node.as_bytes(), snapshot);
    }
    if homes.is_empty() {
        return None;
    }
    Some(Inventory {
        owner: request.owner,
        request_timestamp: request.timestamp,
        homes: homes.into_iter().map(|(node, snapshot)| Home { node, snapshot }).collect(),
    })
}
