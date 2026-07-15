use std::cmp::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::MAX_BOOTSTRAP_RESULTS;
use common::proto::client_res::RelayDescriptor;
use common::quic::xor32;
use common::warn;

use crate::resolver::Resolver;
use crate::resolver::relays::RelayEntry;

pub trait HandleRPC {
    async fn handle_rpc(&self, req: ClientRequest) -> Result<ClientResponse>;
}

impl HandleRPC for Resolver {
    async fn handle_rpc(&self, req: ClientRequest) -> Result<ClientResponse> {
        match req {
            ClientRequest::GetRelays() => {
                let relays: Vec<RelayDescriptor> = self
                    .snapshot_relays()
                    .iter()
                    .map(|r| r.to_descriptor())
                    .collect();
                Ok(ClientResponse::GetRelays { relays })
            },
            ClientRequest::GetBootstrapPeers { near, count_xor_near, count_rtt_near } => {
                handle_get_bootstrap_peers(self, near, count_xor_near, count_rtt_near)
            },
            ClientRequest::GetGateways() => {
                let gateways = self.snapshot_gateways().iter().map(|g| g.to_descriptor()).collect();
                Ok(ClientResponse::GetGateways { gateways })
            },
        }
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
/// **Bounds:** the *combined* count is capped by [`MAX_BOOTSTRAP_RESULTS`].
/// Combined requests above the cap are rejected outright (returning an
/// `anyhow::Error` that the dispatcher above propagates) — accepting them
/// would silently drop the over-budget half and is a programming error
/// on the caller's side worth surfacing. Per-list clamps still apply
/// inside the budget so a single field never asks for more than
/// `MAX_BOOTSTRAP_RESULTS` even when the other list is empty.
fn handle_get_bootstrap_peers(
    resolver: &Resolver, near: [u8; 32], count_xor_near: u8, count_rtt_near: u8,
) -> Result<ClientResponse> {
    // 1. Reject obviously absurd requests early. `u8::checked_add` would
    //    fail at exactly the boundary we want to allow (`32`), so use
    //    `saturating_add` and compare against the cap.
    let combined = count_xor_near.saturating_add(count_rtt_near);
    if combined > MAX_BOOTSTRAP_RESULTS {
        warn!(
            "GetBootstrapPeers rejected: combined count {} exceeds MAX_BOOTSTRAP_RESULTS={}",
            combined, MAX_BOOTSTRAP_RESULTS
        );
        return Err(anyhow!(
            "GetBootstrapPeers: combined count {} > MAX_BOOTSTRAP_RESULTS={}",
            combined,
            MAX_BOOTSTRAP_RESULTS
        ));
    }

    // 2. One snapshot under the read lock — the snapshot is a `Vec` of
    //    cloned `RelayEntry`s (cheap `Arc` clones inside) so subsequent
    //    sort/select work happens with the lock released. Mirrors
    //    `Resolver::snapshot_relays`; we don't reuse it directly only so
    //    we can sort in place without an extra clone.
    let snapshot: Vec<RelayEntry> = resolver.snapshot_relays();

    // 3. XOR-near ranking. Sort the snapshot by ascending distance to
    //    `near`. `sort_by` (not `sort_unstable_by`) for determinism on
    //    ties — two relays at the same distance is statistically
    //    near-impossible at 256 bits but exercising the sort
    //    deterministically makes diffs of test-cluster captures stable.
    let xor_count = (count_xor_near as usize).min(MAX_BOOTSTRAP_RESULTS as usize);
    let mut xor_sorted: Vec<&RelayEntry> = snapshot.iter().collect();
    xor_sorted.sort_by(|a, b| xor_distance_cmp(&near, a, b));
    let xor_near: Vec<RelayDescriptor> = xor_sorted
        .iter()
        .take(xor_count)
        .map(|e| e.to_descriptor())
        .collect();

    // 4. RTT-near ranking — most-recently-heard-from first. See the
    //    function-level doc-comment for the proxy rationale. Reuses the
    //    same snapshot, distinct sort order, no de-dup against the
    //    xor_near list (callers dedupe by `id`).
    let rtt_budget = (MAX_BOOTSTRAP_RESULTS as usize).saturating_sub(xor_near.len());
    let rtt_count = (count_rtt_near as usize).min(rtt_budget);
    let mut rtt_sorted: Vec<&RelayEntry> = snapshot.iter().collect();
    rtt_sorted.sort_by_key(|b| std::cmp::Reverse(b.last_heartbeat_at()));
    let rtt_near: Vec<RelayDescriptor> = rtt_sorted
        .iter()
        .take(rtt_count)
        .map(|e| e.to_descriptor())
        .collect();

    Ok(ClientResponse::GetBootstrapPeers { xor_near, rtt_near })
}

/// Compare two relay entries by XOR distance from `pivot` (ascending).
///
/// The `id` already lives behind a `BaseId<32>` so its `as_bytes()`
/// returns a `&[u8; 32]` — a direct lex compare on the per-byte XOR is
/// equivalent to an unsigned big-endian compare on the 256-bit distance
/// (Kademlia XOR metric).
///
/// Delegates the per-byte XOR to the canonical [`common::quic::xor32`]
/// so we share one implementation across resolver / relay; the array
/// `cmp` then performs the lexicographic compare.
fn xor_distance_cmp(pivot: &[u8; 32], a: &RelayEntry, b: &RelayEntry) -> Ordering {
    let da = xor32(a.id.as_bytes(), pivot);
    let db = xor32(b.id.as_bytes(), pivot);
    da.cmp(&db)
}
