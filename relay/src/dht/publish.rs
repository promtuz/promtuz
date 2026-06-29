//! Publish path: build a [`PresenceRecord`], find the k closest owners,
//! issue parallel `Store` RPCs, tally the outcome.
//!
//! The published-by relay:
//!
//! 1. Runs an iterative `FindNode(target = user_ipk)` walk to obtain the
//!    K closest peers (`super::lookup::lookup_node`).
//! 2. Issues `Store { record }` in parallel against each.
//! 3. Counts `Stored` outcomes; needs `K_MIN` of K to consider the
//!    publish successful.
//! 4. If self is among the K closest, also persists locally via
//!    `super::store::store_record` — short-circuits the network round-trip
//!    against ourselves.
//!
//! ## Why `K_MIN < K`
//!
//! The success threshold is "majority(K)". With K=3 that's 2 — the same
//! number mandated for cross-checked reads. Lower thresholds let publish
//! make progress against a partially-degraded network without sacrificing
//! the cross-check property on the reading side.
//!
//! ## Lock contract
//!
//! `parking_lot` guards are never held across `await`. The lookup-walk
//! is awaited *before* we take any guard; the publish parallelism is
//! driven via `tokio::task::JoinSet` which doesn't require lock holding.

use std::sync::Arc;
use std::time::Duration;

use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::PresenceRecord;
use common::proto::dht_p2p::Store;
use common::proto::dht_p2p::StoreOutcome;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;
use thiserror::Error;
use tokio::time::timeout;

use super::Dht;
use super::config::K;
use super::config::LOOKUP_RPC_TIMEOUT_MS;
use super::lookup::LookupError;
use super::lookup::lookup_node;
use super::store::store_record;

/// Threshold of `Stored` outcomes required for a publish to be
/// considered successful. With `K=3` we accept 2-of-3 — matches the
/// minimum quorum for the read side, so the two halves of the protocol
/// agree on what counts as "the network has accepted this record".
pub const K_MIN: usize = 2;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-replica outcome of a single Store RPC during the publish path.
/// Used for diagnostic / metrics aggregation in [`PublishOutcome`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaResult {
    pub node_id: NodeId,
    pub outcome: StoreOutcome,
}

/// Caller-friendly result of a publish attempt. Distinguishes succeeded
/// from failed without losing the per-replica audit trail.
#[derive(Debug, Clone)]
pub struct PublishOutcome {
    /// Replicas that returned `Stored`. Length is exactly the count of
    /// successful storage operations across self + remotes.
    pub stored_at: Vec<NodeId>,

    /// Replicas that returned anything else (or where the RPC failed to
    /// complete). Including the outcome lets the caller distinguish
    /// "stale, an even fresher record is canonical" from "rate-limited,
    /// retry later".
    pub failed_at: Vec<ReplicaResult>,
}

impl PublishOutcome {
    /// Did we hit the `K_MIN` threshold? Tested by `publish` itself, but
    /// kept on the type so callers that drive their own retry logic can
    /// re-test against an alternate threshold.
    pub fn is_successful(&self) -> bool {
        self.stored_at.len() >= K_MIN
    }
}

/// Failure modes for the publish path.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The `lookup_node` walk failed before we could even discover K
    /// candidates. Caller schedules a retry once routing-table state
    /// settles.
    #[error("publish: lookup of K closest peers failed: {0}")]
    LookupFailed(#[source] LookupError),

    /// We reached K candidates but fewer than `K_MIN` accepted the
    /// record. Caller may schedule a fast retry.
    #[error("publish: insufficient replicas (wanted {wanted}, got {got})")]
    InsufficientReplicas {
        wanted: usize,
        got:    usize,
    },
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the publish workflow for a freshly-built `PresenceRecord`.
///
/// Caller is expected to have:
/// - obtained a fresh `user_sig` from the user inside the authenticated
///   handshake,
/// - constructed the record with `not_before = now`,
///   `not_after = now + PRESENCE_TTL_MS`,
/// - signed `relay_sig` with [`Dht::signing_key`].
pub(crate) async fn publish(
    dht: Arc<Dht>, record: PresenceRecord, now_ms: u64,
) -> Result<PublishOutcome, PublishError> {
    // 1. Find the K closest peers to the record's user_ipk. We use
    //    `lookup_node` (FIND_NODE, not FIND_VALUE) — we want
    //    destinations, not records.
    //
    //    `lookup_node` returns peer descriptors only (it excludes self
    //    from its iteration); the "is self in the top-K?" decision is
    //    made separately below by comparing self's XOR distance to the
    //    K-th descriptor's distance.
    let target_id = NodeId::from_bytes(record.user_ipk.0);
    let descriptors = lookup_node(dht.clone(), target_id)
        .await
        .map_err(PublishError::LookupFailed)?;

    let self_id = dht.node_id;
    let target_bytes = *target_id.as_bytes();

    // 2. Decide whether self is among the K closest. Self is closer
    //    than the worst (K-th) descriptor → self should self-store.
    //    With <K descriptors discovered, we always self-store (the
    //    network is sparse — better to be a replica than to lose data).
    let self_should_store = if descriptors.len() < K {
        true
    } else {
        let self_dist = xor32(self_id.as_bytes(), &target_bytes);
        let kth = &descriptors[K - 1];
        let kth_dist = xor32(kth.id.as_bytes(), &target_bytes);
        self_dist < kth_dist
    };

    // 3. Run STOREs against the top-K peers (which already exclude
    //    self). If we're self-storing, also persist locally.
    let mut stored_at: Vec<NodeId> = Vec::with_capacity(K + 1);
    let mut failed_at: Vec<ReplicaResult> = Vec::new();

    if self_should_store {
        match store_record(&dht, record.clone(), now_ms) {
            StoreOutcome::Stored => stored_at.push(self_id),
            other => failed_at.push(ReplicaResult { node_id: self_id, outcome: other }),
        }
    }

    // Remote stores in parallel against the descriptors we got. If
    // self_should_store and we have K descriptors, this still issues K
    // RPCs — that's K+1 total replicas. Better over-replicated than
    // under.
    let results = remote_store_parallel(&dht, &descriptors, &record).await;
    for r in results {
        match r.outcome {
            StoreOutcome::Stored => stored_at.push(r.node_id),
            _ => failed_at.push(r),
        }
    }

    if stored_at.len() < K_MIN {
        return Err(PublishError::InsufficientReplicas {
            wanted: K_MIN,
            got:    stored_at.len(),
        });
    }

    Ok(PublishOutcome { stored_at, failed_at })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Issue `Store` RPCs against every descriptor in `peers` in parallel,
/// bounded by `LOOKUP_RPC_TIMEOUT_MS` total wall-clock. Each RPC opens
/// its own bi-stream so no peer can head-of-line-block any other.
async fn remote_store_parallel(
    dht: &Arc<Dht>, peers: &[NodeDescriptor], record: &PresenceRecord,
) -> Vec<ReplicaResult> {
    use tokio::task::JoinSet;
    let mut set: JoinSet<ReplicaResult> = JoinSet::new();

    for peer in peers.iter().cloned() {
        let dht_ref = dht.clone();
        let record_clone = record.clone();
        set.spawn(async move {
            let outcome = remote_store_one(&dht_ref, &peer, &record_clone)
                .await
                .unwrap_or(StoreOutcome::BadSig); // any RPC error => treat as a non-success
            ReplicaResult { node_id: peer.id, outcome }
        });
    }

    let mut results = Vec::with_capacity(peers.len());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS);
    while !set.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // Out of budget; surrender any still-in-flight tasks.
            set.abort_all();
            break;
        }
        match timeout(remaining, set.join_next()).await {
            Ok(Some(Ok(r))) => results.push(r),
            // Task panicked or canceled — fold as failure (skip without
            // forging a peer id; we lost which one).
            Ok(Some(Err(_))) => {}
            Ok(None) => break, // set empty
            Err(_) => {
                set.abort_all();
                break;
            }
        }
    }
    results
}

/// Single Store RPC against `peer`. Reuses the cached `peer_conns`
/// connection if one is alive; otherwise opens a fresh one via the
/// shared `lookup::connect_to_peer` path so both modules pull from /
/// populate the same `peer_conns` cache.
async fn remote_store_one(
    dht: &Arc<Dht>, peer: &NodeDescriptor, record: &PresenceRecord,
) -> Result<StoreOutcome, anyhow::Error> {
    use crate::dht::lookup;

    let conn = lookup::connect_to_peer(dht, peer).await?;
    let pkt = DhtPacket::Request(DhtRequest::Store(Store { record: record.clone() }));
    let bytes = pkt.pack()?;

    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&bytes).await?;
    send.finish()?;

    let resp = DhtPacket::unpack(&mut recv).await?;
    match resp {
        DhtPacket::Response(DhtResponse::Store(r)) => Ok(r.outcome),
        // Wrong response type — peer is misbehaving.
        _ => Err(anyhow::anyhow!("publish: peer returned non-Store response")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::dht_p2p::PresenceRecord;
    use common::proto::dht_p2p::presence_record_relay_signing_input;
    use common::proto::dht_p2p::presence_record_user_signing_input;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;

    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-publish-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
    }

    fn build_record(
        user: &SigningKey, relay: &SigningKey, generation: u64, not_before: u64, ttl_ms: u64,
    ) -> PresenceRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let not_after = not_before + ttl_ms;
        let capabilities: u16 = 0;

        let user_msg = presence_record_user_signing_input(&user_ipk, &relay_id, generation);
        let user_sig = user.sign(&user_msg);

        let relay_msg = presence_record_relay_signing_input(
            &user_ipk,
            &relay_id,
            &relay_pubkey,
            not_before,
            not_after,
            generation,
            capabilities,
            &user_sig.to_bytes(),
        );
        let relay_sig = relay.sign(&relay_msg);

        PresenceRecord {
            user_ipk: user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            not_before,
            not_after,
            generation,
            capabilities,
            user_sig: user_sig.to_bytes().into(),
            relay_sig: relay_sig.to_bytes().into(),
        }
    }

    /// A publish against an empty routing table must surface
    /// `PublishError::LookupFailed(LookupError::NoCandidates)` because
    /// `lookup_node` has no shortlist to start from. The rest of the
    /// publish workflow (parallel STOREs, K_MIN threshold) is integration
    /// territory — exercising it requires real QUIC peers, which is
    /// out of scope for the relay's unit-test pass (covered by the
    /// integration suite in `misc/testing/`).
    #[tokio::test(flavor = "current_thread")]
    async fn publish_propagates_lookup_failure_when_routing_empty() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let record = build_record(&user, &relay, 1, now, 600_000);

        let res = publish(dht, record, now + 1).await;
        match res {
            Err(PublishError::LookupFailed(LookupError::NoCandidates)) => {}
            other => panic!("expected LookupFailed(NoCandidates), got {other:?}"),
        }
    }
}
