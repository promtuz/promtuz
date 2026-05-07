//! Sender-side K-closest dispatch fan-out (sticky-home phase 2b).
//!
//! When a relay receives a `Dispatch` from a connected client and the
//! recipient is **not** online locally, the spec
//! (`misc/specs/STICKY_HOME_RELAY.md` §4.2) routes the dispatch to the K
//! "home" relays: the K relays whose NodeIds are closest by XOR to the
//! recipient's `user_ipk`. Each home either delivers locally (recipient
//! online there) or queues the dispatch in `cf_dht_queue` for later
//! pickup.
//!
//! This module implements that fan-out from the *sender* side. It is the
//! sister to [`super::publish::publish`] and deliberately mirrors its
//! shape — same `K_MIN`-quorum success criterion, same `JoinSet`-based
//! parallel dispatch, same self-store short-circuit when the sender
//! relay is itself in the K-closest set.
//!
//! ## Why a separate module instead of merging into `publish.rs`
//!
//! The two paths share the *structure* (parallel RPCs, K_MIN quorum,
//! self-store shortcut) but diverge meaningfully:
//!
//! - `publish` operates on `PresenceRecord` and writes to `cf_presence`;
//!   `forward_to_homes` operates on `DispatchP` and writes to
//!   `cf_dht_queue`.
//! - `publish` returns a single typed outcome enum
//!   (`StoreOutcome::Stored` etc.); `forward_to_homes` distinguishes
//!   `Delivered` from `Stored` because the sender uses that to decide
//!   between [`DispatchAckP::Delivered`] and [`DispatchAckP::Forwarded`]
//!   on the originating client's ack.
//! - `publish` carries an Ed25519-signed presence record; `Forward`
//!   carries an unmodified `DispatchP` plus an *additional* outer
//!   sender-relay signature (the two-layer signing model in §5.1).
//!
//! Sharing the dispatch-parallel idiom with a generic helper would have
//! cost more in indirection than the ~40 lines of duplicated `JoinSet`
//! plumbing, so they live as siblings.
//!
//! ## Lock contract
//!
//! Same as the rest of `dht/`: `parking_lot` guards are never held
//! across `await`. `dht.routing.read().find_closest(...)` is the only
//! routing-table read; we clone descriptors out and release the lock
//! before any I/O.
//!
//! design-doc: `misc/specs/STICKY_HOME_RELAY.md` §4.2 (sender-side flow),
//! §5.1 (`Forward` wire shape), §6.1 (`cf_dht_queue`), §7 question 1
//! (`K_MIN = 2`), §7 question 4 (sender-relay-is-K-closest shortcut).

use std::sync::Arc;
use std::time::Duration;

use common::proto::client_rel::DispatchP;
use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::Forward;
use common::proto::dht_p2p::ForwardOutcome;
use common::proto::dht_p2p::ForwardResp;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::forward_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use ed25519_dalek::Signer;
use thiserror::Error;
use tokio::time::timeout;

use super::Dht;
use super::config::FORWARD_K_MIN;
use super::config::FORWARD_TIMEOUT_MS;
use super::config::K;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-home outcome of a single `Forward` RPC during the fan-out. Used to
/// build the [`ForwardSummary`] tally — the caller (typically
/// `client/events/forward.rs::handle_forward`) only needs the aggregated
/// counts, but per-home audit is preserved for diagnostic logging and
/// metrics correlation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HomeReply {
    pub node_id: NodeId,
    pub outcome: ForwardOutcome,
}

/// Caller-friendly summary of a fan-out attempt. Counts are encoded as
/// per-home `Vec<NodeId>`s rather than scalars so the caller can attribute
/// successes/failures to specific peers when logging or producing
/// observability events.
#[derive(Clone, Debug, Default)]
pub(crate) struct ForwardSummary {
    /// All K homes the sender attempted (including self when self is in
    /// the K-closest set). Length is `K` in the steady-state case;
    /// shorter when the routing table holds fewer than K-1 peers.
    pub homes_tried: Vec<NodeId>,
    /// Homes that returned [`ForwardOutcome::Delivered`] — recipient was
    /// online there and received the dispatch.
    pub delivered_at: Vec<NodeId>,
    /// Homes that returned [`ForwardOutcome::Stored`] — recipient was
    /// offline; the dispatch was durably queued.
    pub stored_at: Vec<NodeId>,
    /// Homes that returned anything else, paired with the outcome for
    /// diagnostic surface.
    pub failed_at: Vec<HomeReply>,
}

impl ForwardSummary {
    /// Sum of `delivered_at + stored_at` — the count of homes that
    /// successfully accepted the dispatch.
    pub fn success_count(&self) -> usize {
        self.delivered_at.len() + self.stored_at.len()
    }

    /// True iff at least one home returned `Delivered`. The sender
    /// promotes its client-side ack from `Forwarded` to `Delivered` in
    /// this case (§4.2 step 6).
    pub fn any_delivered(&self) -> bool {
        !self.delivered_at.is_empty()
    }

    /// True iff `success_count >= FORWARD_K_MIN`.
    pub fn meets_k_min(&self) -> bool {
        self.success_count() >= FORWARD_K_MIN
    }
}

/// Failure modes for the fan-out path. Distinguishes "we couldn't even
/// try" (no homes / no DHT) from "we tried but didn't reach quorum"
/// because the caller wants the same fallback behaviour for both — but
/// metrics and logs benefit from the distinction.
#[derive(Debug, Error)]
pub(crate) enum ForwardError {
    /// `dht.routing.find_closest(target, K)` returned an empty list. The
    /// routing table is empty (bootstrap incomplete) so the fan-out
    /// cannot proceed.
    #[error("forward: routing table empty for target")]
    NoHomes,
    /// `success_count < FORWARD_K_MIN`. Carries the gap so the caller
    /// can include it in fallback-path log messages.
    #[error("forward: insufficient replicas (wanted {wanted}, got {got})")]
    InsufficientReplicas {
        wanted:  usize,
        got:     usize,
        summary: Box<ForwardSummary>,
    },
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the §4.2 sender-side fan-out for a `dispatch` whose recipient is
/// not online on the sending relay.
///
/// 1. Compute the K-closest homes by XOR distance to the recipient's
///    `user_ipk` (raw 32 bytes — same key derivation as `lookup_value`).
/// 2. If `self_id` is among the K-closest, locally enqueue the dispatch
///    via [`super::store::enqueue_for_home`] and treat the outcome as
///    one of the K acks (mirrors the §7 question 4 shortcut and the
///    publish-path's `self_should_store` path).
/// 3. For every remote home, dispatch a `Forward` RPC over `peer/1` in
///    parallel, collecting outcomes via `tokio::task::JoinSet`.
/// 4. Wait up to [`FORWARD_TIMEOUT_MS`] total wall-clock for replies.
/// 5. Tally the `ForwardSummary`; return `Err(InsufficientReplicas)` if
///    the success count is `< FORWARD_K_MIN`.
///
/// **Caller's contract** (typically
/// `relay/src/quic/handler/client/events/forward.rs::handle_forward`):
///
/// - Verifies the embedded `dispatch.sig` *before* calling here (the
///   sender-relay must not forward an unsigned dispatch). The wire-level
///   `Forward::verify` deliberately does not re-check `dispatch.sig`
///   (§5.1 contract); the home relay re-checks at delivery time
///   (phase 2d).
/// - On `Err(_)` falls back to the local `cf_messages` queue safety net
///   (§4.2 step 7).
///
/// **Self-Forward never wraps in a wire `Forward`.** The sender-relay's
/// own `forward_to_homes` shortcuts straight to
/// [`super::store::enqueue_for_home`] when self is in the K-closest set;
/// it does not construct a wire `Forward` for itself, sign it, and feed
/// it through the dispatcher. The wire signature is an attestation of
/// "I, sender_relay, forwarded this to you, home_relay" — pointing at
/// itself would be circular.
pub(crate) async fn forward_to_homes(
    dht: Arc<Dht>, dispatch: DispatchP, now_ms: u64,
) -> Result<ForwardSummary, ForwardError> {
    dht.metrics.inc_forwards_sent();

    // 1. Compute the K-closest homes. The user's IPK is the DHT key
    //    directly (§1 of `DHT.md` — "users' DHT keys are their raw
    //    IPK", *not* `BLAKE3(IPK)`); same derivation `lookup_value`
    //    uses for `FindValue`.
    let user_ipk_bytes: [u8; 32] = dispatch.to.0;
    let target_id = NodeId::from_bytes(user_ipk_bytes);

    let descriptors: Vec<NodeDescriptor> = {
        let routing = dht.routing.read();
        routing.find_closest(&target_id, K)
    };

    // Decide whether self is among the K-closest using the same XOR
    // comparison the publish path uses. `find_closest` excludes self,
    // so we need a separate self-vs-Kth-distance check.
    let self_id = dht.node_id;
    let self_is_in_k = if descriptors.len() < K {
        // Sparse routing table — be permissive (self is "trivially"
        // in the K-closest because there aren't K others). Mirrors
        // the same permissiveness in `store::self_is_owner` and
        // `publish::self_should_store`.
        true
    } else {
        let self_dist = xor_dist(self_id.as_bytes(), &user_ipk_bytes);
        let kth = &descriptors[K - 1];
        let kth_dist = xor_dist(kth.id.as_bytes(), &user_ipk_bytes);
        self_dist < kth_dist
    };

    if descriptors.is_empty() && !self_is_in_k {
        // Routing table is empty AND self isn't a home (impossible in
        // practice: empty routing → permissive self_is_in_k → branch
        // unreachable, but the type system doesn't know that). Surface
        // explicitly so the caller can fall back to local queue.
        return Err(ForwardError::NoHomes);
    }
    if descriptors.is_empty() && self_is_in_k {
        // Lone-relay edge case: the §4.5 "Cold network (1 relay)" row.
        // We're our own K. Self-store, treat as 1-of-K success even
        // though K_MIN=2 means we still fall back to local queue —
        // consistency with the K_MIN contract. The caller's local-
        // queue safety net catches this.
    }

    // 2. Self-store short-circuit (§7 question 4). If self is in the
    //    K-closest, we add a self-record to the summary without dialing
    //    ourselves over the network.
    let mut summary = ForwardSummary::default();
    let mut homes_tried: Vec<NodeId> = Vec::with_capacity(K + 1);

    if self_is_in_k {
        homes_tried.push(self_id);
        let outcome =
            super::store::enqueue_for_home(&dht, &user_ipk_bytes, &dispatch, now_ms);
        match outcome {
            ForwardOutcome::Stored => summary.stored_at.push(self_id),
            other => summary.failed_at.push(HomeReply { node_id: self_id, outcome: other }),
        }
    }

    // 3. Build the wire `Forward` once — the same `Forward` is sent to
    //    every remote home (one signature, K-1 transmissions) since
    //    the transcript covers `(dispatch.id, sender_relay_id, timestamp)`,
    //    none of which depend on the home being addressed. This matches
    //    publish.rs's "build record once, multiplex over K peers"
    //    pattern.
    let forward_pkt = build_signed_forward(&dht, dispatch, now_ms);

    // 4. Fan-out RPCs against the K-1 (or K) remote descriptors in
    //    parallel, bounded by [`FORWARD_TIMEOUT_MS`] total wall-clock.
    let remote_replies = remote_forward_parallel(&dht, &descriptors, &forward_pkt).await;

    for reply in remote_replies {
        homes_tried.push(reply.node_id);
        match reply.outcome {
            ForwardOutcome::Delivered => summary.delivered_at.push(reply.node_id),
            ForwardOutcome::Stored => summary.stored_at.push(reply.node_id),
            other => summary.failed_at.push(HomeReply {
                node_id: reply.node_id,
                outcome: other,
            }),
        }
    }
    summary.homes_tried = homes_tried;

    // 5. Quorum decision.
    if summary.success_count() < FORWARD_K_MIN {
        let got = summary.success_count();
        return Err(ForwardError::InsufficientReplicas {
            wanted:  FORWARD_K_MIN,
            got,
            summary: Box::new(summary),
        });
    }

    if summary.any_delivered() {
        dht.metrics.inc_forwards_delivered();
    } else {
        dht.metrics.inc_forwards_stored();
    }

    Ok(summary)
}

/// Construct a fully-signed [`Forward`] for `dispatch` using `dht.signing_key`
/// — the relay's identity key, **not** the TLS sub-key (per §4.2 sig domain
/// notes; the identity key is what the home relay's `Forward::verify`
/// pulls from the routing-table entry for `sender_relay_id`).
///
/// Mirrors the resolver-link signing pattern in
/// `relay/src/quic/resolver_link.rs::send_heartbeat` — same
/// `signing_key.sign(&signing_input).to_bytes()` shape.
fn build_signed_forward(dht: &Dht, dispatch: DispatchP, timestamp: u64) -> Forward {
    let sender_relay_id = dht.node_id;
    let msg = forward_signing_input(&dispatch.id.0, &sender_relay_id, timestamp);
    let sig = dht.signing_key.sign(&msg).to_bytes();
    Forward {
        dispatch,
        sender_relay_id,
        timestamp,
        sig: sig.into(),
    }
}

/// 32-byte XOR distance, big-endian-comparable. Same helper shape as
/// `publish.rs::xor_dist`, kept private to this module to avoid a thin
/// `pub(super)` export for a 4-line function.
fn xor_dist(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

// ---------------------------------------------------------------------------
// Remote fan-out
// ---------------------------------------------------------------------------

/// Issue `Forward` RPCs against every descriptor in `peers` in parallel,
/// bounded by [`FORWARD_TIMEOUT_MS`] total wall-clock. Each RPC opens
/// its own bi-stream so no peer can head-of-line-block any other.
///
/// Returns a per-peer reply for every peer that responded inside the
/// budget. Peers whose RPCs timed out, panicked, or whose connection
/// failed are *omitted* from the result rather than recorded as a
/// synthetic failure outcome — letting the caller's tally treat
/// "no response" identically to "no entry in the result set". The
/// summary's `homes_tried` list is computed at the call-site so the
/// caller doesn't lose track of who was attempted.
async fn remote_forward_parallel(
    dht: &Arc<Dht>, peers: &[NodeDescriptor], forward: &Forward,
) -> Vec<HomeReply> {
    use tokio::task::JoinSet;
    let mut set: JoinSet<Option<HomeReply>> = JoinSet::new();

    for peer in peers.iter().cloned() {
        let dht_ref = dht.clone();
        let forward_clone = forward.clone();
        set.spawn(async move {
            let outcome = remote_forward_one(&dht_ref, &peer, &forward_clone).await;
            outcome.map(|o| HomeReply { node_id: peer.id, outcome: o })
        });
    }

    let mut results = Vec::with_capacity(peers.len());
    let deadline = tokio::time::Instant::now() + Duration::from_millis(FORWARD_TIMEOUT_MS);
    while !set.is_empty() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            // Out of budget; surrender any still-in-flight tasks. The
            // home will eventually respond into a closed stream — that's
            // a fact-of-life with timeout-bounded RPCs and matches the
            // publish-path's same surrender behaviour.
            set.abort_all();
            break;
        }
        match timeout(remaining, set.join_next()).await {
            Ok(Some(Ok(Some(r)))) => results.push(r),
            Ok(Some(Ok(None))) => {} // RPC failed without an outcome
            Ok(Some(Err(_))) => {}    // task panicked or canceled
            Ok(None) => break,         // set empty
            Err(_) => {
                set.abort_all();
                break;
            }
        }
    }
    results
}

/// Single `Forward` RPC against `peer`. Reuses the cached `peer_conns`
/// connection if alive; otherwise opens a fresh one via the shared
/// `lookup::connect_to_peer` path so this module pulls from / populates
/// the same `peer_conns` cache as the publish/lookup paths.
///
/// Returns `Some(outcome)` on a structurally valid round-trip and
/// `None` on any RPC-level failure (connect failed, write failed,
/// response was the wrong variant). The caller treats `None` as
/// "this home contributed nothing to the K_MIN tally".
async fn remote_forward_one(
    dht: &Arc<Dht>, peer: &NodeDescriptor, forward: &Forward,
) -> Option<ForwardOutcome> {
    let conn = super::lookup::connect_to_peer(dht, peer).await.ok()?;

    let pkt = DhtPacket::Request(DhtRequest::Forward(forward.clone()));
    let bytes = pkt.pack().ok()?;

    let (mut send, mut recv) = conn.open_bi().await.ok()?;
    send.write_all(&bytes).await.ok()?;
    send.finish().ok()?;

    let resp = DhtPacket::unpack(&mut recv).await.ok()?;
    match resp {
        DhtPacket::Response(DhtResponse::Forward(ForwardResp { outcome })) => Some(outcome),
        // Wrong response variant — peer is misbehaving. We deliberately
        // do *not* close the connection here: the peer's misbehaviour
        // will surface again on the next RPC and the per-peer rate
        // limiter on the inbound side will eventually trip. Closing
        // optimistically would create connect/disconnect storms under
        // a buggy-but-not-malicious peer.
        _ => None,
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

    use common::proto::client_rel::DispatchP;
    use common::proto::client_rel::dispatch_sig_message;
    use common::proto::dht_p2p::ForwardOutcome;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;
    use crate::dht::dht_cf_descriptors;

    /// Counter-derived signing key. Matches the discipline established
    /// in `publish.rs::tests::fresh_signing_key` — distinct keys per
    /// call without an RNG dep.
    fn fresh_signing_key() -> SigningKey {
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let n = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let mut seed = [0u8; 32];
        seed[..8].copy_from_slice(&n.to_le_bytes());
        seed[31] = (n & 0xff) as u8;
        seed[16] = ((n >> 8) & 0xff) as u8;
        SigningKey::from_bytes(&seed)
    }

    fn fresh_dht(self_id: NodeId) -> Arc<Dht> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("promtuz-fwd-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let mut opts = rust_rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let mut cfs = vec![rust_rocksdb::ColumnFamilyDescriptor::new(
            "default",
            rust_rocksdb::Options::default(),
        )];
        cfs.extend(dht_cf_descriptors());

        let db = rust_rocksdb::DB::open_cf_descriptors(&opts, &path, cfs).expect("open db");
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, Arc::new(db)).expect("dht"))
    }

    /// Build a fresh, internally-consistent `DispatchP` from `from_user`
    /// to `to_user`. Mirrors the `build_dispatch` helper in
    /// `common/src/proto/dht_p2p.rs`'s test module.
    fn build_dispatch(
        from_user: &SigningKey, to_ipk: &[u8; 32], id: [u8; 16], payload: &[u8],
    ) -> DispatchP {
        let from_ipk: [u8; 32] = from_user.verifying_key().to_bytes();
        let msg = dispatch_sig_message(to_ipk, &from_ipk, &id, payload);
        let sig = from_user.sign(&msg);
        DispatchP {
            to:      (*to_ipk).into(),
            from:    from_ipk.into(),
            id:      id.into(),
            payload: payload.to_vec().into(),
            sig:     sig.to_bytes().into(),
        }
    }

    // -------------------------------------------------------------------
    // ForwardSummary tally arithmetic — pure-function tests
    // -------------------------------------------------------------------

    fn id_for(n: u8) -> NodeId {
        let mut b = [0u8; 32];
        b[0] = n;
        NodeId::new(b)
    }

    #[test]
    fn forward_summary_empty_has_zero_success_and_no_delivered() {
        let s = ForwardSummary::default();
        assert_eq!(s.success_count(), 0);
        assert!(!s.any_delivered());
        assert!(!s.meets_k_min());
    }

    #[test]
    fn forward_summary_one_stored_does_not_meet_k_min() {
        let mut s = ForwardSummary::default();
        s.stored_at.push(id_for(1));
        assert_eq!(s.success_count(), 1);
        assert!(!s.any_delivered());
        // K_MIN = 2, so 1 stored alone does not meet the threshold.
        assert!(!s.meets_k_min());
    }

    #[test]
    fn forward_summary_two_stored_meets_k_min_no_delivered() {
        let mut s = ForwardSummary::default();
        s.stored_at.push(id_for(1));
        s.stored_at.push(id_for(2));
        assert_eq!(s.success_count(), 2);
        assert!(!s.any_delivered());
        assert!(s.meets_k_min());
    }

    #[test]
    fn forward_summary_one_delivered_one_stored_meets_k_min_with_delivered() {
        let mut s = ForwardSummary::default();
        s.delivered_at.push(id_for(1));
        s.stored_at.push(id_for(2));
        assert_eq!(s.success_count(), 2);
        assert!(s.any_delivered());
        assert!(s.meets_k_min());
    }

    #[test]
    fn forward_summary_failed_outcomes_do_not_count_toward_quorum() {
        // Two failures + one success = 1 success; below K_MIN=2 so the
        // tally must reject. Specifically catches a regression where
        // `failed_at.len()` accidentally feeds into `success_count`.
        let mut s = ForwardSummary::default();
        s.stored_at.push(id_for(1));
        s.failed_at.push(HomeReply {
            node_id: id_for(2),
            outcome: ForwardOutcome::NotOwner,
        });
        s.failed_at.push(HomeReply {
            node_id: id_for(3),
            outcome: ForwardOutcome::QueueFull,
        });
        assert_eq!(s.success_count(), 1);
        assert!(!s.meets_k_min());
    }

    #[test]
    fn forward_summary_all_delivered_meets_k_min() {
        // Edge case: every home delivered locally (recipient is online
        // on multiple homes — possible during reconnection windows).
        let mut s = ForwardSummary::default();
        for n in 1..=3 {
            s.delivered_at.push(id_for(n));
        }
        assert_eq!(s.success_count(), 3);
        assert!(s.any_delivered());
        assert!(s.meets_k_min());
    }

    // -------------------------------------------------------------------
    // forward_to_homes — integration with empty routing table
    // -------------------------------------------------------------------

    /// `forward_to_homes` against an empty routing table on a fresh DHT
    /// must succeed via the self-store short-circuit (§4.5 "cold
    /// network" + §7 question 4: self counts as 1-of-K) — but with
    /// only 1 success and `K_MIN = 2`, the tally fails and we return
    /// `InsufficientReplicas`. This is the canonical lone-relay
    /// fallback signal: the caller (handle_forward) sees it and
    /// stores in the local-queue safety net.
    #[tokio::test(flavor = "current_thread")]
    async fn forward_to_homes_lone_relay_returns_insufficient_replicas_with_self_stored() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let dispatch = build_dispatch(&from_user, &to_ipk, [1u8; 16], b"hi");

        let now: u64 = 1_700_000_000_000;
        let res = forward_to_homes(dht.clone(), dispatch, now).await;
        match res {
            Err(ForwardError::InsufficientReplicas { wanted, got, summary }) => {
                assert_eq!(wanted, FORWARD_K_MIN);
                // Self-store is the sole success.
                assert_eq!(got, 1);
                assert_eq!(summary.stored_at, vec![dht.node_id]);
                assert!(summary.delivered_at.is_empty());
            }
            other => panic!("expected InsufficientReplicas, got {other:?}"),
        }
    }

    /// Verifies the self-store path actually wrote into `cf_dht_queue`.
    /// Catches a regression where the self-store branch silently no-ops
    /// the on-disk write (e.g. if the `enqueue_for_home` call were
    /// replaced by an unimplemented stub).
    #[tokio::test(flavor = "current_thread")]
    async fn forward_to_homes_self_store_actually_writes_cf_dht_queue() {
        use crate::dht::store::CF_DHT_QUEUE;

        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let from_user = fresh_signing_key();
        let to_user = fresh_signing_key();
        let to_ipk: [u8; 32] = to_user.verifying_key().to_bytes();
        let dispatch = build_dispatch(&from_user, &to_ipk, [42u8; 16], b"persisted");

        let now: u64 = 1_700_000_000_000;
        let _ = forward_to_homes(dht.clone(), dispatch.clone(), now).await;

        // Confirm a key with the recipient's IPK as prefix exists in
        // `cf_dht_queue`. The exact key shape is `MessageKey { recipient
        // = to_ipk, ts_ms = now, dispatch_id = [42; 16] }` per
        // `enqueue_for_home`.
        let cf = dht.rocks.cf_handle(CF_DHT_QUEUE).expect("cf");
        let mut found = false;
        for entry in dht.rocks.prefix_iterator_cf(&cf, &to_ipk) {
            let (k, _) = entry.expect("iter");
            if k.starts_with(&to_ipk) {
                found = true;
                break;
            }
        }
        assert!(found, "self-store must have written to cf_dht_queue");
    }
}
