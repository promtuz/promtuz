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
                let gateways = self.snapshot_gateways().iter().map(|g| g.to_descriptor()).collect();
                Ok(Arc::new(ClientResponse::GetGateways { gateways }.pack()?))
            },
        }
    }
}

impl Resolver {
    /// Packed `GetRelays` response for the current registry generation.
    ///
    /// The whole directory serialises to ~100 KiB at `MAX_RELAYS`, so it is
    /// built once per membership change and handed out as a shared buffer;
    /// a request flood then costs one atomic read and an `Arc` clone.
    fn relays_response(&self) -> Result<Arc<Vec<u8>>> {
        let generation = self.relays_generation.load(AtomicOrdering::Acquire);

        if let Some((cached, packet)) = self.relays_response.read().as_ref()
            && *cached == generation
        {
            return Ok(packet.clone());
        }

        let relays: Vec<RelayDescriptor> =
            self.relays.read().values().map(RelayEntry::to_descriptor).collect();
        let packet = Arc::new(ClientResponse::GetRelays { relays }.pack()?);
        *self.relays_response.write() = Some((generation, packet.clone()));

        Ok(packet)
    }
}

/// Implementation of [`ClientRequest::GetBootstrapPeers`].
///
/// **Auth:** none. This is a public query; the response is a strict
/// subset of what `GetRelays` already exposes.
///
/// **Strategy:** snapshot the registry once, then perform two separate
/// rankings on the snapshot:
///
/// 1. `xor_near`: ascending by `dist(near, entry.id) = near ^ entry.id`,
///    sorted lex over the 32-byte distance. Mirrors the per-bucket
///    selection that the requesting relay would do locally if it
///    already had a populated routing table.
///
/// 2. `rtt_near`: descending by `last_heartbeat_at` (most-recently
///    active first). The resolver does not measure relay-to-relay RTT
///    yet — recency-of-liveness is a documented proxy. A relay that
///    just sent a heartbeat is by definition still healthy and routable
///    from the resolver's vantage point, which is a useful seed for a
///    fresh-joiner.
///
/// **Bounds:** the *combined* count is capped by [`MAX_BOOTSTRAP_RESULTS`]
/// (see [`bootstrap_counts`]), and each ranking is a partial selection over
/// the snapshot, so per-request work scales with the requested count rather
/// than with the registry size.
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

    let mut snapshot = resolver.snapshot_relays();

    let xor_near =
        select_top(&mut snapshot, xor_count, |a, b| xor_distance_cmp(&near, &a.id, &b.id));
    let rtt_near = select_top(&mut snapshot, rtt_count, |a, b| {
        b.last_heartbeat_at().cmp(&a.last_heartbeat_at())
    });

    Ok(ClientResponse::GetBootstrapPeers { xor_near, rtt_near })
}

/// Per-list result counts inside the [`MAX_BOOTSTRAP_RESULTS`] budget.
///
/// A combined request over the cap is an error rather than a silent trim:
/// accepting it would drop the over-budget half without telling the caller.
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

/// The `count` best entries under `order`, ranked. Partitions `entries` in
/// place so only the returned head is sorted; the tail is left unordered.
fn select_top<F>(entries: &mut [RelayEntry], count: usize, order: F) -> Vec<RelayDescriptor>
where
    F: Fn(&RelayEntry, &RelayEntry) -> Ordering,
{
    let count = count.min(entries.len());
    if count == 0 {
        return Vec::new();
    }

    entries.select_nth_unstable_by(count - 1, &order);
    let (head, _) = entries.split_at_mut(count);
    head.sort_by(&order);
    head.iter().map(RelayEntry::to_descriptor).collect()
}

/// Compare two relay ids by XOR distance from `pivot` (ascending).
///
/// A direct lex compare on the per-byte XOR is equivalent to an unsigned
/// big-endian compare on the 256-bit distance (Kademlia XOR metric).
fn xor_distance_cmp(pivot: &[u8; 32], a: &RelayId, b: &RelayId) -> Ordering {
    xor32(a.as_bytes(), pivot).cmp(&xor32(b.as_bytes(), pivot))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(seed: u8) -> RelayId {
        let mut b = [0u8; 32];
        b[0] = seed;
        RelayId::from_bytes(b)
    }

    #[test]
    fn bootstrap_counts_reject_a_combined_request_over_the_cap() {
        assert!(bootstrap_counts(MAX_BOOTSTRAP_RESULTS, 1).is_err());
        assert!(bootstrap_counts(255, 255).is_err());
    }

    #[test]
    fn bootstrap_counts_allow_exactly_the_cap() {
        let split = MAX_BOOTSTRAP_RESULTS / 2;
        let budget = MAX_BOOTSTRAP_RESULTS as usize;
        let counts = bootstrap_counts(split, MAX_BOOTSTRAP_RESULTS - split).ok();
        assert_eq!(counts, Some((split as usize, budget - split as usize)));
    }

    #[test]
    fn bootstrap_counts_clamp_one_list_to_the_whole_budget() {
        let budget = MAX_BOOTSTRAP_RESULTS as usize;
        assert_eq!(bootstrap_counts(MAX_BOOTSTRAP_RESULTS, 0).ok(), Some((budget, 0)));
    }

    #[test]
    fn xor_distance_orders_by_closeness_to_the_pivot() {
        let pivot = [0u8; 32];
        assert_eq!(xor_distance_cmp(&pivot, &id(1), &id(2)), Ordering::Less);
        assert_eq!(xor_distance_cmp(&pivot, &id(9), &id(9)), Ordering::Equal);
        assert_eq!(xor_distance_cmp(id(3).as_bytes(), &id(3), &id(0)), Ordering::Less);
    }
}
