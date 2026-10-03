use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::RelayId;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::MAX_BOOTSTRAP_RESULTS;
use common::proto::client_res::RelayDescriptor;
use common::proto::client_res::StoreDescriptor;
use common::proto::pack::Packer;
use common::quic::xor32;

use crate::resolver::Resolver;
use crate::resolver::relays::RelayEntry;

pub trait HandleRPC {
    /// Framed response bytes, ready to write to the requesting stream.
    async fn handle_rpc(&self, req: ClientRequest) -> Result<Arc<Vec<u8>>>;
}

impl HandleRPC for Resolver {
    async fn handle_rpc(&self, req: ClientRequest) -> Result<Arc<Vec<u8>>> {
        match req {
            ClientRequest::GetRelays() => self.relays_response(),
            ClientRequest::GetBootstrapPeers { near, count_xor_near, count_rtt_near } => {
                let res = handle_get_bootstrap_peers(self, near, count_xor_near, count_rtt_near)?;
                Ok(Arc::new(res.pack()?))
            },
            ClientRequest::GetGateways() => {
                let gateways = self.gateways.descriptors();
                Ok(Arc::new(ClientResponse::GetGateways { gateways }.pack()?))
            },
            ClientRequest::GetStores() => {
                let stores = self
                    .cfg
                    .store
                    .iter()
                    .map(|s| StoreDescriptor {
                        id:       s.id,
                        base_url: s.base_url.trim_end_matches('/').to_string(),
                    })
                    .collect();
                Ok(Arc::new(ClientResponse::GetStores { stores }.pack()?))
            },
        }
    }
}

impl Resolver {
    /// Built once per membership change: the directory packs to roughly 100 KiB at `MAX_RELAYS`.
    fn relays_response(&self) -> Result<Arc<Vec<u8>>> {
        let generation = self.relays.generation.load(AtomicOrdering::Acquire);

        if let Some((cached, packet)) = self.relays_response.read().as_ref()
            && *cached == generation
        {
            return Ok(packet.clone());
        }

        let relays = self.relays.descriptors();
        let packet = Arc::new(ClientResponse::GetRelays { relays }.pack()?);
        *self.relays_response.write() = Some((generation, packet.clone()));

        Ok(packet)
    }
}

/// `rtt_near` ranks by heartbeat recency; the resolver does not measure RTT.
fn handle_get_bootstrap_peers(
    resolver: &Resolver, near: [u8; 32], count_xor_near: u8, count_rtt_near: u8,
) -> Result<ClientResponse> {
    let (xor_count, rtt_count) = bootstrap_counts(count_xor_near, count_rtt_near)?;
    if xor_count == 0 && rtt_count == 0 {
        return Ok(ClientResponse::GetBootstrapPeers {
            xor_near: Vec::new(),
            rtt_near: Vec::new(),
        });
    }

    let mut snapshot = resolver.relays.snapshot();

    let by_distance = |a: &RelayEntry, b: &RelayEntry| xor_distance_cmp(&near, &a.id, &b.id);
    let by_recency =
        |a: &RelayEntry, b: &RelayEntry| b.last_heartbeat_at().cmp(&a.last_heartbeat_at());
    let descriptors = |top: &[RelayEntry]| -> Vec<RelayDescriptor> {
        top.iter().map(RelayEntry::to_descriptor).collect()
    };
    let xor_near = descriptors(select_top(&mut snapshot, xor_count, by_distance));
    let rtt_near = descriptors(select_top(&mut snapshot, rtt_count, by_recency));

    Ok(ClientResponse::GetBootstrapPeers { xor_near, rtt_near })
}

fn bootstrap_counts(count_xor_near: u8, count_rtt_near: u8) -> Result<(usize, usize)> {
    let combined = count_xor_near.saturating_add(count_rtt_near);
    if combined > MAX_BOOTSTRAP_RESULTS {
        return Err(anyhow!(
            "GetBootstrapPeers: combined count {combined} > MAX_BOOTSTRAP_RESULTS={MAX_BOOTSTRAP_RESULTS}"
        ));
    }

    let budget = MAX_BOOTSTRAP_RESULTS as usize;
    let xor = (count_xor_near as usize).min(budget);
    let rtt = (count_rtt_near as usize).min(budget.saturating_sub(xor));

    Ok((xor, rtt))
}

fn select_top<T, F>(entries: &mut [T], count: usize, order: F) -> &[T]
where
    F: Fn(&T, &T) -> Ordering,
{
    let count = count.min(entries.len());
    if count == 0 {
        return &[];
    }

    entries.select_nth_unstable_by(count - 1, &order);
    let (head, _) = entries.split_at_mut(count);
    head.sort_by(&order);
    head
}

/// Lexicographic order on the XOR bytes is big-endian order on the 256-bit distance.
fn xor_distance_cmp(pivot: &[u8; 32], a: &RelayId, b: &RelayId) -> Ordering {
    xor32(a.as_bytes(), pivot).cmp(&xor32(b.as_bytes(), pivot))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(first: u8) -> RelayId {
        let mut bytes = [0; 32];
        bytes[0] = first;
        RelayId::from_bytes(bytes)
    }

    #[test]
    fn bootstrap_peers_come_nearest_first_within_the_budget() {
        let cap = MAX_BOOTSTRAP_RESULTS;
        assert!(bootstrap_counts(cap, 1).is_err());
        assert!(bootstrap_counts(u8::MAX, u8::MAX).is_err());
        assert_eq!(bootstrap_counts(cap, 0).ok(), Some((cap as usize, 0)));
        assert_eq!(bootstrap_counts(3, cap - 3).ok(), Some((3, cap as usize - 3)));

        let pivot = id(4);
        let nearest = |count| {
            let mut ids = [0x80, 1, 4, 0x40, 6].map(id);
            select_top(&mut ids, count, |a, b| xor_distance_cmp(pivot.as_bytes(), a, b)).to_vec()
        };
        assert_eq!(nearest(2), [4, 6].map(id));
        assert_eq!(nearest(10), [4, 6, 1, 0x40, 0x80].map(id));
        assert!(nearest(0).is_empty());
    }
}
