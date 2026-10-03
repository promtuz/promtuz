//! Bootstrap: seed the routing table from the resolver, then walk a `FindNode` for this relay's own
//! id. Failure is not fatal; the scheduler retries while the table is sparse.

use std::sync::Arc;

use common::proto::client_res::RelayDescriptor;
use common::proto::dht_p2p::NodeDescriptor;
use common::quic::id::NodeId;
use thiserror::Error;

use super::Dht;
use super::routing::InsertOutcome;
use crate::quic::resolver_link::ResolverLinkHandle;

const BOOTSTRAP_COUNT_XOR_NEAR: u8 = 8;
const BOOTSTRAP_COUNT_RTT_NEAR: u8 = 4;

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("bootstrap: resolver returned an empty registry")]
    EmptyRegistry,

    #[error("bootstrap: resolver request failed: {0}")]
    Resolver(#[source] anyhow::Error),
}

pub async fn bootstrap(
    dht: Arc<Dht>, resolver: ResolverLinkHandle,
) -> Result<(), BootstrapError> {
    crate::dht_log!(
        "DHT bootstrap: querying resolver for {} XOR-near + {} RTT-near peers",
        BOOTSTRAP_COUNT_XOR_NEAR, BOOTSTRAP_COUNT_RTT_NEAR
    );

    let near = *dht.node_id.as_bytes();
    let (xor_near, rtt_near) = resolver
        .get_bootstrap_peers(near, BOOTSTRAP_COUNT_XOR_NEAR, BOOTSTRAP_COUNT_RTT_NEAR)
        .await
        .map_err(BootstrapError::Resolver)?;

    if xor_near.is_empty() && rtt_near.is_empty() {
        return Err(BootstrapError::EmptyRegistry);
    }

    // The resolver's two rankings can name the same peer.
    let mut seen = std::collections::HashSet::with_capacity(xor_near.len() + rtt_near.len());
    let mut to_insert: Vec<NodeDescriptor> = Vec::with_capacity(seen.capacity());
    let mut unbound = 0usize;
    for rd in xor_near.iter().chain(rtt_near.iter()) {
        if !seen.insert(rd.id) {
            continue;
        }
        match node_descriptor_from(rd) {
            Some(desc) => to_insert.push(desc),
            None => unbound += 1,
        }
    }

    let mut inserted = 0usize;
    let mut refreshed = 0usize;
    let mut deferred = 0usize;
    let mut self_skipped = 0usize;
    let mut pending: Vec<InsertOutcome> = Vec::new();
    {
        let mut routing = dht.routing.write();
        for desc in to_insert {
            match routing.insert(desc) {
                InsertOutcome::Inserted => inserted += 1,
                InsertOutcome::Refreshed => refreshed += 1,
                InsertOutcome::Discarded => deferred += 1,
                outcome @ InsertOutcome::PendingPing(_) => {
                    deferred += 1;
                    pending.push(outcome);
                },
                InsertOutcome::IsSelf => self_skipped += 1,
            }
        }
    }
    for outcome in pending {
        crate::dht::lookup::probe_pending_ping(&dht, outcome);
    }

    crate::dht_log!(
        "DHT bootstrap: inserted={}, refreshed={}, deferred={}, unbound={}, self={} (xor_near={}, rtt_near={})",
        inserted,
        refreshed,
        deferred,
        unbound,
        self_skipped,
        xor_near.len(),
        rtt_near.len()
    );

    // Each peer the walk dials adds this relay to its table, which seeding alone cannot do, and
    // joins this one. A failed walk only logs.
    match crate::dht::lookup::lookup_node(dht.clone(), dht.node_id).await {
        Ok(peers) => crate::dht_log!("DHT bootstrap: self-FindNode converged on {} peer(s)", peers.len()),
        Err(e) => crate::dht_log!("DHT bootstrap: self-FindNode walk failed: {e}; proceeding with seeded routing"),
    }

    crate::dht_log!("DHT bootstrap: complete (resolver seed + self-FindNode convergence)");
    Ok(())
}

/// The resolver is not trusted to bind keys to ids: an unchecked binding would let it install an
/// attacker's key for an honest relay's id, and later signature checks would accept the attacker.
fn node_descriptor_from(rd: &RelayDescriptor) -> Option<NodeDescriptor> {
    if NodeId::new(rd.pubkey.0) != rd.id {
        common::warn!("DHT bootstrap: resolver descriptor for {} is not key-bound; dropping", rd.id);
        return None;
    }
    Some(NodeDescriptor { id: rd.id, addr: rd.addr, pubkey: rd.pubkey })
}
