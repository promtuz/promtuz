//! Shared Tier-2 fan-out primitives for the Phase 9 MLS originate
//! helpers (`mls_kp_originate`, `mls_welcome_originate`).
//!
//! When a phone delegates a KeyPackage / Welcome operation to its home
//! relay over the `client/0` §3.9 wrappers, the home relay becomes the
//! *originator* of the real `peer/1` DHT fan-out — the role libcore's
//! deleted `Peer1DhtClient` used to play. These helpers are the generic
//! analogue of `forward.rs`'s `Forward`-specialised
//! `remote_forward_one` / `remote_forward_parallel`: they work over any
//! [`DhtRequest`] / [`DhtResponse`] pair so the KP and Welcome modules
//! don't each re-implement the connect → pack → round-trip → collect
//! dance.
//!
//! The originating relay carries the phone's *inner* user signature
//! verbatim inside each request (it cannot forge one), so the K storage
//! homes verify the user exactly as they would for a relay-to-relay
//! dial — the home is a forwarder, never a trust root.

use std::sync::Arc;
use std::time::Duration;

use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;
use tokio::time::timeout;

use super::Dht;
use super::config::FORWARD_TIMEOUT_MS;
use super::config::K;

/// Compute the K-closest homes for `target_id` plus whether *self* is
/// among them. Mirrors the self-in-K decision in
/// `forward.rs::forward_to_homes`: `find_closest` excludes self, so we
/// compare self's XOR distance against the K-th descriptor's. A routing
/// table with fewer than K entries is treated permissively (self is
/// "trivially" a home), matching `store::self_is_owner` /
/// `publish::self_should_store`.
///
/// The returned `Vec<NodeDescriptor>` is the *remote* set to dial; the
/// bool says whether the caller should additionally self-store via the
/// inbound handler. The routing read-guard is dropped before return so
/// callers never hold it across an `await` (parking_lot rule).
pub(crate) fn closest_homes_with_self(
    dht: &Arc<Dht>, target_id: &NodeId,
) -> (Vec<NodeDescriptor>, bool) {
    let descriptors: Vec<NodeDescriptor> = {
        let routing = dht.routing.read();
        routing.find_closest(target_id, K)
    };

    let self_in_k = if descriptors.len() < K {
        true
    } else {
        let self_dist = xor32(dht.node_id.as_bytes(), target_id.as_bytes());
        let kth_dist = xor32(descriptors[K - 1].id.as_bytes(), target_id.as_bytes());
        self_dist < kth_dist
    };

    (descriptors, self_in_k)
}

/// Issue a single DHT-RPC against `peer`, reusing / populating the
/// shared `peer_conns` cache via `lookup::connect_to_peer`. Returns the
/// raw [`DhtResponse`] on a structurally valid round-trip, or `None` on
/// any RPC-level failure (connect, write, decode). The caller matches
/// the expected response variant. Clone of `forward.rs::remote_forward_one`
/// generalised off the `Forward` type.
pub(crate) async fn remote_rpc_one(
    dht: &Arc<Dht>, peer: &NodeDescriptor, req: &DhtRequest,
) -> Option<DhtResponse> {
    let conn = super::lookup::connect_to_peer(dht, peer).await.ok()?;

    let bytes = DhtPacket::Request(req.clone()).pack().ok()?;
    let (mut send, mut recv) = conn.open_bi().await.ok()?;
    send.write_all(&bytes).await.ok()?;
    send.finish().ok()?;

    match DhtPacket::unpack(&mut recv).await.ok()? {
        DhtPacket::Response(resp) => Some(resp),
        // Wrong frame kind — peer misbehaving. Don't close the
        // connection (mirrors `remote_forward_one`'s rationale: avoid
        // connect/disconnect storms under a buggy-not-malicious peer).
        _ => None,
    }
}

/// Fan a request out to every `peer` in parallel, bounded by
/// [`FORWARD_TIMEOUT_MS`] total wall-clock, and collect every raw
/// [`DhtResponse`] that arrived inside the budget. Peers that timed
/// out, failed to connect, or returned a malformed frame are simply
/// absent from the result — the caller's quorum tally treats "no
/// response" as "contributed nothing". Clone of
/// `forward.rs::remote_forward_parallel` generalised off `Forward`.
///
/// Use for the *quorum* and *merge* RPCs (publish / refill /
/// welcome-publish / welcome-fetch / welcome-ack). Do **not** use for
/// KeyPackage *fetch* — a fetch consumes a one-shot KP slot at every
/// home it reaches, so the fetch path dials sequentially and stops at
/// the first `Found` (see `mls_kp_originate::originate_fetch`).
pub(crate) async fn fan_out_collect(
    dht: &Arc<Dht>, peers: &[NodeDescriptor], req: &DhtRequest,
) -> Vec<DhtResponse> {
    use tokio::task::JoinSet;
    let mut set: JoinSet<Option<DhtResponse>> = JoinSet::new();

    for peer in peers {
        let dht_ref = dht.clone();
        let req_clone = req.clone();
        let peer = peer.clone();
        set.spawn(async move { remote_rpc_one(&dht_ref, &peer, &req_clone).await });
    }

    let mut results = Vec::with_capacity(peers.len());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(FORWARD_TIMEOUT_MS);
    while !set.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            set.abort_all();
            break;
        }
        match timeout(remaining, set.join_next()).await {
            Ok(Some(Ok(Some(r)))) => results.push(r),
            Ok(Some(Ok(None))) => {}  // RPC failed without a response
            Ok(Some(Err(_))) => {}    // task panicked / cancelled
            Ok(None) => break,        // set drained
            Err(_) => {
                set.abort_all();
                break;
            }
        }
    }
    results
}
