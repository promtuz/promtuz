//! Iterative `FindNode` / `FindValue` walks with α=3 parallelism and
//! per-hop hedging.
//!
//! ## Algorithm
//!
//! We maintain three logical sets:
//!
//! - **`pending`**: the candidate shortlist of peers we *might* query,
//!   sorted by XOR distance to `target`.
//! - **`in_flight`**: the peers we've sent a request to and are still
//!   waiting for. Bounded at `α = 3`.
//! - **`queried`**: peers that have already responded (or been hedged-out).
//!
//! Termination:
//! 1. We've contacted the K strictly-closest peers in `pending` and none
//!    returns a closer-than-current peer, OR
//! 2. `LOOKUP_MAX_HOPS` exceeded, OR
//! 3. `LOOKUP_RPC_TIMEOUT_MS` total wall-clock elapsed.
//!
//! ## Hedging
//!
//! When a request hasn't returned within `LOOKUP_HEDGE_MS`, we *don't*
//! cancel it — instead we fire a duplicate to the next-best candidate.
//! Whichever responds first wins; the loser's reply (if it eventually
//! arrives) is folded back into the candidate pool opportunistically.
//!
//! ## Lock contract
//!
//! Like the rest of `dht/`, we never hold a `parking_lot` guard across
//! `await`. The `routing.read().find_closest(...)` call is the only
//! routing-table read; we clone the descriptors out and release the
//! lock before any I/O.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FindNode;
use common::proto::dht_p2p::FindValue;
use common::proto::dht_p2p::FindValueOutcome as WireFindValueOutcome;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::PresenceRecord;
use common::proto::dht_p2p::dht_hello_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::quic::xor32;
use common::types::bytes::Bytes;
use ed25519_dalek::Signer;
use quinn::Connection;
use thiserror::Error;
use tokio::time::timeout;

use super::Dht;
use super::config::ALPHA;
use super::config::K;
use super::config::LOOKUP_HEDGE_MS;
use super::config::LOOKUP_MAX_HOPS;
use super::config::LOOKUP_QUORUM;
use super::config::LOOKUP_RPC_TIMEOUT_MS;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Outcome of an iterative `FindValue` walk.
//
// Variant size: `Found(PresenceRecord)` is ~250 B while `NotPresent` is
// zero-sized. Boxing `PresenceRecord` would shrink the enum but every
// caller in the lookup path then needs an extra deref; revisit once the
// access pattern is concrete.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum FindValueOutcome {
    /// We collected at least one `Found(record)` reply. The returned
    /// record is the highest-priority winner under the canonical ordering.
    Found(PresenceRecord),

    /// All reachable closest peers reported `NotPresent` — authoritative
    /// "user is offline".
    NotPresent,
}

/// Failure modes for an iterative lookup.
#[derive(Debug, Error)]
pub enum LookupError {
    /// Routing table empty — bootstrap has not yet completed.
    #[error("lookup: no candidates in routing table (bootstrap not done?)")]
    NoCandidates,

    /// Total wall-clock budget exhausted before convergence.
    #[error("lookup: timed out after {LOOKUP_RPC_TIMEOUT_MS}ms")]
    Timeout,

    /// `LOOKUP_MAX_HOPS` exceeded without termination — typically means a
    /// network partition or hostile peers feeding loops.
    #[error("lookup: exceeded {LOOKUP_MAX_HOPS} hops")]
    MaxHopsExceeded,

    /// Failed to open a peer connection (TLS, handshake, DNS). Surfaced
    /// only when *every* candidate failed — a single peer failure is
    /// folded into the iteration silently.
    #[error("lookup: peer connect failed: {0}")]
    PeerConnect(#[source] anyhow::Error),
}

// ---------------------------------------------------------------------------
// Internal candidate state
// ---------------------------------------------------------------------------

/// Candidate peer in the lookup shortlist, decorated with its XOR
/// distance to the lookup target so we can sort cheaply.
#[derive(Clone, Debug)]
struct Candidate {
    desc:     NodeDescriptor,
    distance: [u8; 32],
}

fn distance(target: &[u8; 32], peer: &NodeId) -> [u8; 32] {
    xor32(target, peer.as_bytes())
}

// ---------------------------------------------------------------------------
// Connection management
// ---------------------------------------------------------------------------

/// Open (or reuse) a QUIC connection to `peer`. Cached in `dht.peer_conns`
/// alongside the verified Ed25519 cert pubkey extracted post-handshake.
///
/// On any cached-but-dead connection, we evict and re-dial. Drops are
/// cheap because the inner `quinn::Connection` is `Arc`-shared internally.
///
/// **TLS pubkey check:** after the handshake completes, we extract the
/// server's leaf-cert SPKI via
/// [`crate::dht::tls_extract::extract_and_verify_pubkey`] and
/// **reject** the connection if `BLAKE3(spki) != peer.id`. The TLS
/// layer already validated the cert chain against the configured root
/// CA; this post-handshake check is defense-in-depth for the
/// (hypothetical) scenario where a CA mints a cert whose SPKI does
/// not match the relay's NodeId. On rejection we close with
/// `CloseReason::DhtMalformedKey` and bump
/// `metrics.cert_pubkey_extraction_failures`.
///
/// **Application-layer signed handshake:** before returning
/// the connection to the caller, we send a [`DhtHello`] (Ed25519-signed
/// transcript binding our `node_id` to our `pubkey` and a fresh
/// timestamp) on a fresh uni-stream. The receiver verifies and uses the
/// bound NodeId for routing-table inserts and rate-limit keying for
/// the rest of the connection's lifetime — this closes the
/// inbound-no-mTLS gap without enabling mTLS on `peer/1` (which would
/// break the `client/1` co-tenant on the same `Endpoint`).
///
/// **Fire-and-forget hello, no synchronous ack.** We send the hello on
/// a uni-stream and return the connection immediately; if the peer
/// rejects, the next RPC's bi-stream will fail because the connection
/// is closed. This matches the relay-to-resolver `RelayHello` flow
/// (`relay/src/quic/resolver_link.rs::hello`) — adding a synchronous
/// ack would cost a round-trip per dial without any extra security
/// (the dialer can't *act* on the ack: a successful subsequent RPC
/// proves the peer accepted the hello, and a failed RPC could equally
/// be caused by the receiver rejecting the hello mid-flight).
///
/// **TODO (integration suite).** The full QUIC-stream-level test of
/// "outbound dial → DhtHello uni-stream → inbound accept_uni → verify →
/// bi-stream RPC" needs real connections; today we test the
/// signing/verification helper round-trip in isolation
/// (`common::proto::dht_p2p::tests::dht_hello_two_relays_authenticate
/// _each_other_synchronously`) and rely on the unit tests of each
/// half. A real two-relay harness covers the rest.
///
/// Visible to the rest of `dht/` (notably `publish.rs`) so the cache +
/// dial path is shared rather than duplicated.
pub(crate) async fn connect_to_peer(
    dht: &Arc<Dht>, peer: &NodeDescriptor,
) -> anyhow::Result<Connection> {
    // Fast path: hit the cache.
    if let Some((conn, _pk)) = dht.peer_conns.read().get(&peer.id).cloned()
        && conn.close_reason().is_none() {
            return Ok(conn);
        }

    let endpoint = match dht.endpoint.as_ref() {
        Some(ep) => ep.clone(),
        None => return Err(anyhow::anyhow!("DHT has no endpoint configured")),
    };
    let client_cfg = match dht.peer_client_cfg.as_ref() {
        Some(cfg) => cfg.clone(),
        None => return Err(anyhow::anyhow!("DHT has no peer_client_cfg configured")),
    };

    let sni = peer.id.to_string();
    let conn = endpoint
        .connect_with(client_cfg.as_ref().clone(), peer.addr, &sni)?
        .await?;

    // Post-handshake cert-pubkey extraction + binding check (item 1).
    let verified_pubkey = match crate::dht::tls_extract::extract_and_verify_pubkey(&conn, &peer.id) {
        Ok(pk) => pk,
        Err(e) => {
            dht.metrics.inc_cert_pubkey_extraction_failures();
            common::warn!(
                "DHT connect_to_peer: post-handshake pubkey extraction failed for {}: {e}",
                peer.id
            );
            common::quic::CloseReason::DhtMalformedKey.close(&conn);
            return Err(anyhow::anyhow!(
                "post-handshake pubkey check failed for {}: {e}",
                peer.id
            ));
        }
    };

    // Send our signed `DhtHello` as the first frame on the connection.
    // Failure here is non-fatal-to-the-handshake (the peer will simply
    // close on its end), but we surface it so the dialer sees the
    // failure and the caller can decide whether to retry.
    if let Err(e) = send_dht_hello(dht, &conn).await {
        dht.metrics.inc_dht_hello_rejected();
        common::warn!(
            "DHT connect_to_peer: failed to send DhtHello to {}: {e}; closing",
            peer.id
        );
        common::quic::CloseReason::DhtMalformedKey.close(&conn);
        return Err(anyhow::anyhow!(
            "failed to send DhtHello to {}: {e}",
            peer.id
        ));
    }

    // Cache. Race: another task may have raced ahead with a connection
    // to the same peer; if so, drop the loser. Both `Connection`s are
    // independently usable — the eventual consistency is only about
    // *which* one future calls reuse.
    {
        let mut conns = dht.peer_conns.write();
        if let Some((existing, _)) = conns.get(&peer.id).cloned()
            && existing.close_reason().is_none() {
                return Ok(existing);
            }
        conns.insert(peer.id, (conn.clone(), verified_pubkey));
    }
    dht.metrics.inc_peer_conns_opened();

    // Bidirectional: serve inbound RPCs on this outbound connection too, so
    // the peer can reuse it to call us back (the `peer_conns` cache is shared
    // across both directions). The peer's identity is the dial's verified
    // cert NodeId-binding — no second `DhtHello` needed. Spawned only on a
    // fresh dial; the fast-path reuse above already has a serve loop.
    tokio::spawn(crate::dht::handler::serve_peer_streams(
        dht.clone(),
        conn.clone(),
        crate::dht::handler::AuthenticatedPeer::new(peer.id, verified_pubkey),
    ));

    Ok(conn)
}

/// Send our signed [`DhtHello`] on a freshly-opened uni-stream. The
/// transcript is built via [`dht_hello_signing_input`], so dialer
/// (this) and receiver (`relay/src/dht/handler.rs::recv_and_verify_hello`)
/// always agree byte-for-byte.
///
/// Lives next to [`connect_to_peer`] (rather than a free fn in
/// `dht/mod.rs`) because it's the only call-site and stays close to
/// the dial-path it serves.
async fn send_dht_hello(dht: &Arc<Dht>, conn: &Connection) -> anyhow::Result<()> {
    let node_id = dht.node_id;
    // The dialer's own pubkey: derivable from the signing key.
    let pubkey: [u8; 32] = dht.signing_key.verifying_key().to_bytes();
    let timestamp = now_ms();
    let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp);
    let sig = dht.signing_key.sign(&msg).to_bytes();

    let hello = DhtHello {
        node_id,
        pubkey: Bytes(pubkey),
        timestamp,
        sig: Bytes(sig),
    };
    let bytes = hello.pack()?;

    let mut send = conn.open_uni().await?;
    send.write_all(&bytes).await?;
    send.finish()?;
    Ok(())
}

/// Wall-clock now in ms-since-Unix-epoch. Inlined here (rather than
/// pulled from `crate::util::systime`) for the same reason as
/// `handler.rs::now_ms` — keeps `dht::lookup` free of cross-module
/// dependencies for a one-line helper. Tests get to override at the
/// caller via `DhtHello`'s `timestamp` field, never via this helper.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Send one DHT request over a fresh bi-stream and read the response.
///
/// Send one DHT request over a fresh bi-stream: open_bi → write request →
/// finish() the send side → read length-prefixed response → done.
///
/// Wraps the entire round-trip in a `LOOKUP_RPC_TIMEOUT_MS` deadline so
/// a single slow peer can't stall the iteration past its budget.
async fn rpc_one(conn: &Connection, req: DhtRequest) -> anyhow::Result<DhtResponse> {
    let pkt = DhtPacket::Request(req);
    let bytes = pkt.pack()?;

    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&bytes).await?;
    send.finish()?;

    let resp = DhtPacket::unpack(&mut recv).await?;
    match resp {
        DhtPacket::Response(r) => Ok(r),
        DhtPacket::Request(_) => {
            Err(anyhow::anyhow!("rpc_one: peer sent a Request where a Response was expected"))
        }
    }
}

// ---------------------------------------------------------------------------
// FindNode iterative walk
// ---------------------------------------------------------------------------

/// Iterative `FindNode` walk to discover the k closest peers to `target`.
///
/// Used by:
/// - bootstrap's "self-FindNode" forced-convergence step,
/// - the publish path to find STORE recipients,
/// - bucket-refresh to re-discover stale ranges.
///
/// Returns the top-k peers by XOR distance the walk converged on.
pub(crate) async fn lookup_node(
    dht: Arc<Dht>, target: NodeId,
) -> Result<Vec<NodeDescriptor>, LookupError> {
    dht.metrics.inc_lookups_started();

    let target_bytes = *target.as_bytes();

    // Seed the shortlist from the routing table. No `await` between
    // taking the read guard and dropping it — clone descriptors out.
    let initial: Vec<NodeDescriptor> = {
        let routing = dht.routing.read();
        routing.find_closest(&target, K * 2)
    };

    if initial.is_empty() {
        dht.metrics.inc_lookups_failed();
        return Err(LookupError::NoCandidates);
    }

    // The "shortlist" — kept sorted by distance ascending.
    let mut candidates: Vec<Candidate> = initial
        .into_iter()
        .map(|desc| {
            let distance = distance(&target_bytes, &desc.id);
            Candidate { desc, distance }
        })
        .collect();
    candidates.sort_by_key(|a| a.distance);

    let mut queried: HashSet<NodeId> = HashSet::new();
    queried.insert(dht.node_id); // never query self
    let mut closest_so_far: Vec<Candidate> = Vec::with_capacity(K);

    let deadline = Instant::now() + Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS);
    let mut hops: u32 = 0;

    let res = run_iterative_loop(
        &dht,
        &target_bytes,
        &mut candidates,
        &mut queried,
        &mut closest_so_far,
        &mut hops,
        deadline,
        false, // is_value_lookup = false
    )
    .await;

    match res {
        Ok(_) => {
            dht.metrics.inc_lookups_succeeded();
            Ok(closest_so_far.into_iter().take(K).map(|c| c.desc).collect())
        }
        Err(IterError::ValueFound(_)) => {
            // Cannot happen — `lookup_node` sends `FindNode` only.
            unreachable!("FindNode walk surfaced a ValueFound branch")
        }
        Err(IterError::Lookup(e)) => {
            dht.metrics.inc_lookups_failed();
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// FindValue iterative walk
// ---------------------------------------------------------------------------

/// Iterative `FindValue` walk for `user_ipk`.
///
/// Same structure as `lookup_node` but each hop sends `FindValue` and
/// honours `Found` / `NotPresent` / `Closer`.
///
/// **Quorum behaviour (Sybil/eclipse mitigation):**
///
/// - Continue iterating even after a `Found` reply arrives — collect
///   replies from all K-closest peers (or as many as respond before
///   the deadline / max-hop bound).
/// - When the iteration terminates, hand the collected replies to
///   [`decide_lookup_outcome`], which returns `Found` only if at least
///   [`LOOKUP_QUORUM`] peers agree on `(generation, relay_id)`.
///   Otherwise return `NotPresent` even if a single peer claimed
///   `Found` — the lone-hit scenario is treated as a likely eclipse
///   attempt rather than trusted blindly.
/// - Among the `Found` replies that exceed quorum, the highest
///   `(generation, not_before, relay_id)` wins per the canonical
///   ordering on `PresenceRecord::compare`.
///
/// **Tradeoff:** a record just published — stored only on its first
/// replica so far, before anti-entropy has propagated — triggers the
/// lone-hit path and returns `NotPresent` for up to one
/// `ANTI_ENTROPY_INTERVAL_MS` window (30 s). The publishing relay is
/// the canonical home and any follow-up Dispatch into it succeeds via
/// the local-first short-circuit; lookups from other relays see
/// `NotPresent` until anti-entropy spreads the record.
pub(crate) async fn lookup_value(
    dht: Arc<Dht>, user_ipk: [u8; 32],
) -> Result<FindValueOutcome, LookupError> {
    dht.metrics.inc_lookups_started();

    // Bootstrap the candidate set the same way as `lookup_node`.
    let target_id = NodeId::from_bytes(user_ipk);

    let initial: Vec<NodeDescriptor> = {
        let routing = dht.routing.read();
        routing.find_closest(&target_id, K * 2)
    };

    if initial.is_empty() {
        dht.metrics.inc_lookups_failed();
        return Err(LookupError::NoCandidates);
    }

    let mut candidates: Vec<Candidate> = initial
        .into_iter()
        .map(|desc| {
            let distance = distance(&user_ipk, &desc.id);
            Candidate { desc, distance }
        })
        .collect();
    candidates.sort_by_key(|a| a.distance);

    let mut queried: HashSet<NodeId> = HashSet::new();
    queried.insert(dht.node_id);
    let mut closest_so_far: Vec<Candidate> = Vec::with_capacity(K);

    let deadline = Instant::now() + Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS);
    let mut hops: u32 = 0;

    // Collected replies for the quorum decision. We accumulate here
    // (rather than aborting on the first Found, like `lookup_node`)
    // so the cross-check can run on a complete picture.
    let mut value_replies: Vec<ValueReply> = Vec::new();

    let res = run_iterative_loop_value(
        &dht,
        &user_ipk,
        &mut candidates,
        &mut queried,
        &mut closest_so_far,
        &mut hops,
        deadline,
        &mut value_replies,
    )
    .await;

    match res {
        Ok(()) => {
            // The walk converged — apply the quorum decision.
            dht.metrics.inc_lookups_succeeded();
            Ok(decide_lookup_outcome(value_replies))
        }
        Err(e) => {
            dht.metrics.inc_lookups_failed();
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared iterative loop
// ---------------------------------------------------------------------------

/// Internal control-flow error from `run_iterative_loop`. Splits the
/// "caller-visible failure" from the "found-the-value, exit early"
/// signal — the latter is only used by `lookup_node`'s shape; the
/// value-lookup path uses a separate driver that accumulates replies
/// for the quorum decision (`run_iterative_loop_value`).
enum IterError {
    /// A `Found(record)` arrived during a node lookup. Cannot occur in
    /// practice — node lookups never see `FindValueFound` results
    /// because they send `FindNode`, not `FindValue`. Kept as a
    /// hard-unreachable on the code side for clarity.
    ValueFound(Box<PresenceRecord>),
    /// A genuine lookup failure (timeout, max-hops, etc.).
    Lookup(LookupError),
}

/// Drive the α-parallel iterative loop with hedging. Used by
/// `lookup_node`. The value-lookup variant (`lookup_value`) uses
/// [`run_iterative_loop_value`] which accumulates `Found` replies for
/// the quorum decision.
///
/// Returns `Ok(())` when the walk converged peacefully
/// (`closest_so_far` is now populated), `Err(IterError::Lookup(_))` for
/// failures.
//
// Eight args is the iterative-walk state (target + four mutable
// candidate sets + deadline + is-value flag). Bundling into a
// `LookupCtx` struct would split borrow patterns awkwardly across
// the loop body — every iteration would need disjoint mutable
// borrows on three of the fields. Allow the arity.
#[allow(clippy::too_many_arguments)]
async fn run_iterative_loop(
    dht: &Arc<Dht>, target: &[u8; 32], candidates: &mut Vec<Candidate>,
    queried: &mut HashSet<NodeId>, closest_so_far: &mut Vec<Candidate>, hops: &mut u32,
    deadline: Instant, is_value_lookup: bool,
) -> Result<(), IterError> {
    use tokio::task::JoinSet;

    loop {
        if Instant::now() >= deadline {
            return Err(IterError::Lookup(LookupError::Timeout));
        }
        if *hops >= LOOKUP_MAX_HOPS {
            return Err(IterError::Lookup(LookupError::MaxHopsExceeded));
        }

        // 1. Snapshot the next α candidates that haven't been queried.
        let mut batch: Vec<NodeDescriptor> = Vec::with_capacity(ALPHA);
        for c in candidates.iter() {
            if !queried.contains(&c.desc.id) {
                batch.push(c.desc.clone());
                if batch.len() >= ALPHA {
                    break;
                }
            }
        }
        if batch.is_empty() {
            // Nothing left to query — loop has converged.
            return Ok(());
        }

        // 2. Fire α requests in parallel.
        let mut set: JoinSet<RpcResult> = JoinSet::new();
        for desc in batch.iter() {
            queried.insert(desc.id);
            let dht_ref = dht.clone();
            let desc_clone = desc.clone();
            let target_arr = *target;
            let is_value = is_value_lookup;
            set.spawn(async move {
                send_one_hop(&dht_ref, desc_clone, target_arr, is_value).await
            });
        }

        let hedge_window = Duration::from_millis(LOOKUP_HEDGE_MS);
        let mut got_one = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                set.abort_all();
                return Err(IterError::Lookup(LookupError::Timeout));
            }
            let wait = if got_one { hedge_window.min(remaining) } else { remaining };

            let joined = match timeout(wait, set.join_next()).await {
                Ok(j) => j,
                Err(_) => {
                    if got_one {
                        break;
                    } else {
                        continue;
                    }
                }
            };
            let Some(task_result) = joined else {
                break;
            };
            let result = match task_result {
                Ok(r) => r,
                Err(_join) => continue,
            };
            got_one = true;
            match result {
                RpcResult::FindNodeReply(closer) => {
                    integrate_descriptors(target, candidates, &closer);
                }
                RpcResult::FindValueClose(_)
                | RpcResult::FindValueFound(_)
                | RpcResult::FindValueNotPresent => {
                    // Should not occur — `lookup_node` sends FindNode
                    // exclusively, so the responder cannot return any
                    // FindValue variant. Defensive: ignore.
                }
                RpcResult::Failed => {}
            }
        }

        *hops += 1;
        candidates.sort_by_key(|a| a.distance);
        closest_so_far.clear();
        for c in candidates.iter().take(K) {
            closest_so_far.push(c.clone());
        }
        let farthest = closest_so_far.last().map(|c| c.distance);
        let any_closer_unqueried = candidates.iter().any(|c| {
            !queried.contains(&c.desc.id)
                && farthest.map(|f| c.distance < f).unwrap_or(true)
        });
        if !any_closer_unqueried {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Value-lookup driver (quorum)
// ---------------------------------------------------------------------------

/// One peer's reply to a `FindValue` issued during the iterative walk.
/// The collection of these is fed into [`decide_lookup_outcome`] for
/// the quorum decision.
//
// Same shape as the wire `FindValueOutcome`; we keep the
// `Found(PresenceRecord)` inline for the same reason — the
// iterative loop holds at most α=3 of these in `replies` at once
// and the comparison code reads through `Found(rec)` directly.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum ValueReply {
    /// Peer claimed it has the record. Carried so the quorum can
    /// compare `(generation, relay_id)` across replies.
    Found(PresenceRecord),
    /// Peer is in the k-closest but reports no record.
    NotPresent,
}

/// Drive the α-parallel iterative loop for `lookup_value`, accumulating
/// `Found`/`NotPresent` replies in `replies` for the eventual quorum
/// decision. Termination matches the standard Kademlia rule but **does
/// not short-circuit on a `Found`** — every peer that responds before the
/// deadline contributes to the decision.
//
// Same eight-arg shape as `run_iterative_loop`; same rationale —
// a `LookupCtx` struct would fight the disjoint-mutable-borrow
// pattern in the loop body.
#[allow(clippy::too_many_arguments)]
async fn run_iterative_loop_value(
    dht: &Arc<Dht>, target: &[u8; 32], candidates: &mut Vec<Candidate>,
    queried: &mut HashSet<NodeId>, closest_so_far: &mut Vec<Candidate>, hops: &mut u32,
    deadline: Instant, replies: &mut Vec<ValueReply>,
) -> Result<(), LookupError> {
    use tokio::task::JoinSet;

    loop {
        if Instant::now() >= deadline {
            return Err(LookupError::Timeout);
        }
        if *hops >= LOOKUP_MAX_HOPS {
            return Err(LookupError::MaxHopsExceeded);
        }

        let mut batch: Vec<NodeDescriptor> = Vec::with_capacity(ALPHA);
        for c in candidates.iter() {
            if !queried.contains(&c.desc.id) {
                batch.push(c.desc.clone());
                if batch.len() >= ALPHA {
                    break;
                }
            }
        }
        if batch.is_empty() {
            return Ok(());
        }

        let mut set: JoinSet<RpcResult> = JoinSet::new();
        for desc in batch.iter() {
            queried.insert(desc.id);
            let dht_ref = dht.clone();
            let desc_clone = desc.clone();
            let target_arr = *target;
            set.spawn(async move {
                send_one_hop(&dht_ref, desc_clone, target_arr, true).await
            });
        }

        let hedge_window = Duration::from_millis(LOOKUP_HEDGE_MS);
        let mut got_one = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                set.abort_all();
                return Err(LookupError::Timeout);
            }
            let wait = if got_one { hedge_window.min(remaining) } else { remaining };
            let joined = match timeout(wait, set.join_next()).await {
                Ok(j) => j,
                Err(_) => {
                    if got_one {
                        break;
                    } else {
                        continue;
                    }
                }
            };
            let Some(task_result) = joined else {
                break;
            };
            let result = match task_result {
                Ok(r) => r,
                Err(_) => continue,
            };
            got_one = true;
            match result {
                RpcResult::FindValueClose(closer)
                | RpcResult::FindNodeReply(closer) => {
                    integrate_descriptors(target, candidates, &closer);
                }
                RpcResult::FindValueFound(record) => {
                    replies.push(ValueReply::Found(record));
                }
                RpcResult::FindValueNotPresent => {
                    replies.push(ValueReply::NotPresent);
                }
                RpcResult::Failed => {}
            }
        }

        *hops += 1;
        candidates.sort_by_key(|a| a.distance);
        closest_so_far.clear();
        for c in candidates.iter().take(K) {
            closest_so_far.push(c.clone());
        }
        let farthest = closest_so_far.last().map(|c| c.distance);
        let any_closer_unqueried = candidates.iter().any(|c| {
            !queried.contains(&c.desc.id)
                && farthest.map(|f| c.distance < f).unwrap_or(true)
        });
        if !any_closer_unqueried {
            return Ok(());
        }
    }
}

/// Pure quorum decision (Sybil/eclipse mitigation). Extracted from
/// `lookup_value` so it can be unit-tested without a network stack.
///
/// Decision rules:
/// 1. Group `Found` replies by `(generation, relay_id)`. Among groups
///    of size `>= LOOKUP_QUORUM`, pick the one whose record wins the
///    canonical ordering (`PresenceRecord::compare`).
/// 2. If no group reaches quorum, return `NotPresent` — even if a
///    single peer claimed `Found`. The lone-hit scenario is the
///    eclipse threat.
pub(crate) fn decide_lookup_outcome(replies: Vec<ValueReply>) -> FindValueOutcome {
    if replies.is_empty() {
        return FindValueOutcome::NotPresent;
    }

    // Group `Found` replies by their (generation, relay_id) pair —
    // the quorum "agreement" definition. We keep the *highest* record
    // within each group so two peers with the same generation but
    // slightly different `not_before` republish times still cluster
    // together (the higher `not_before` wins inside the group, which
    // converges to the same canonical answer).
    use std::collections::HashMap;
    let mut groups: HashMap<(u64, common::proto::RelayId), Vec<PresenceRecord>> = HashMap::new();
    let mut not_present_count = 0usize;
    for r in replies {
        match r {
            ValueReply::Found(rec) => {
                let key = (rec.generation, rec.relay_id);
                groups.entry(key).or_default().push(rec);
            }
            ValueReply::NotPresent => not_present_count += 1,
        }
    }

    // Pick the largest group that reaches quorum. Ties between groups of
    // equal size break by the canonical ordering on the group's canonical
    // record, so two relays that disagree on the exact record produce the
    // same deterministic winner across all observers.
    let mut quorum_winner: Option<PresenceRecord> = None;
    let mut winner_group_size: usize = 0;
    for (_key, recs) in groups.into_iter() {
        let count = recs.len();
        if count < LOOKUP_QUORUM {
            continue;
        }
        // Pick the canonical winner inside the group via
        // `PresenceRecord::compare`.
        let mut best = recs[0].clone();
        for r in &recs[1..] {
            if r.compare(&best) == std::cmp::Ordering::Greater {
                best = r.clone();
            }
        }
        let prefer_new = match &quorum_winner {
            None => true,
            Some(_) if count > winner_group_size => true,
            Some(prev) if count == winner_group_size => {
                best.compare(prev) == std::cmp::Ordering::Greater
            }
            _ => false,
        };
        if prefer_new {
            quorum_winner = Some(best);
            winner_group_size = count;
        }
    }

    if let Some(rec) = quorum_winner {
        FindValueOutcome::Found(rec)
    } else {
        // Either no `Found` at all (everyone said `NotPresent`) or
        // the lone-hit eclipse scenario. Both map to `NotPresent`.
        let _ = not_present_count;
        FindValueOutcome::NotPresent
    }
}

/// One-hop RPC outcome. We collapse the wire `DhtResponse` into a small
/// enum for the iterative loop's match arm.
//
// `FindValueFound(PresenceRecord)` is large; the other variants are
// `Vec<NodeDescriptor>` (heap-indirected, small inline). Boxing
// would force an extra heap hop on every found-record reply, which
// is the hot path for successful lookups.
#[allow(clippy::large_enum_variant)]
enum RpcResult {
    FindNodeReply(Vec<NodeDescriptor>),
    FindValueClose(Vec<NodeDescriptor>),
    FindValueFound(PresenceRecord),
    FindValueNotPresent,
    Failed,
}

/// Connect (or reuse) and issue one DHT RPC against `peer`.
async fn send_one_hop(
    dht: &Arc<Dht>, peer: NodeDescriptor, target: [u8; 32], is_value_lookup: bool,
) -> RpcResult {
    let conn = match connect_to_peer(dht, &peer).await {
        Ok(c) => c,
        Err(_) => return RpcResult::Failed,
    };

    let req = if is_value_lookup {
        DhtRequest::FindValue(FindValue {
            user_ipk:  target.into(),
            requester: dht.node_id,
        })
    } else {
        DhtRequest::FindNode(FindNode {
            target:    target.into(),
            requester: dht.node_id,
        })
    };

    let resp = match timeout(
        Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS),
        rpc_one(&conn, req),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(_)) | Err(_) => return RpcResult::Failed,
    };

    match resp {
        DhtResponse::FindNode(r) => RpcResult::FindNodeReply(r.closer),
        DhtResponse::FindValue(r) => match r.result {
            WireFindValueOutcome::Found(rec) => RpcResult::FindValueFound(rec),
            WireFindValueOutcome::NotPresent => RpcResult::FindValueNotPresent,
            WireFindValueOutcome::Closer(c) => RpcResult::FindValueClose(c),
        },
        // Wrong response variant — peer is misbehaving. Treat as failure.
        _ => RpcResult::Failed,
    }
}

/// Merge a peer's reply descriptors into the candidate pool, dropping
/// duplicates (already in `candidates`). Each new entry gets its
/// distance computed once.
fn integrate_descriptors(
    target: &[u8; 32], candidates: &mut Vec<Candidate>, new: &[NodeDescriptor],
) {
    let known: HashSet<NodeId> = candidates.iter().map(|c| c.desc.id).collect();
    for desc in new {
        if known.contains(&desc.id) {
            continue;
        }
        let dist = distance(target, &desc.id);
        candidates.push(Candidate { desc: desc.clone(), distance: dist });
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
        let path = std::env::temp_dir().join(format!("promtuz-lookup-test-{pid}-{id}"));
        let _ = std::fs::remove_dir_all(&path);

        let store = Arc::new(crate::storage::db::Store::open(&path).expect("open store"));
        let signing = fresh_signing_key();
        let cfg = DhtConfig::default();
        Arc::new(Dht::new(self_id, signing, cfg, store).expect("dht"))
    }

    /// `lookup_node` against an empty routing table must return
    /// `LookupError::NoCandidates` immediately — the rest of the
    /// iterative algorithm cannot be exercised without spinning up
    /// real QUIC connections (out of scope for unit tests; integration
    /// tests cover it).
    #[tokio::test(flavor = "current_thread")]
    async fn lookup_node_with_empty_routing_table_returns_no_candidates() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let mut target_seed = [0u8; 32];
        target_seed[0] = 2;
        let target = NodeId::new(target_seed);

        let result = lookup_node(dht, target).await;
        assert!(matches!(result, Err(LookupError::NoCandidates)));
    }

    /// `lookup_value` mirrors the same guard.
    #[tokio::test(flavor = "current_thread")]
    async fn lookup_value_with_empty_routing_table_returns_no_candidates() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let user_ipk = [7u8; 32];
        let result = lookup_value(dht, user_ipk).await;
        assert!(matches!(result, Err(LookupError::NoCandidates)));
    }

    // -----------------------------------------------------------------
    // Quorum decision tests
    // -----------------------------------------------------------------

    use common::proto::dht_p2p::PresenceRecord;
    use common::proto::dht_p2p::presence_record_relay_signing_input;
    use common::proto::dht_p2p::presence_record_user_signing_input;
    use ed25519_dalek::Signer;

    /// Build a record matching what the handler/store tests use, but
    /// minimised — we only feed it through `decide_lookup_outcome`,
    /// which never touches signatures.
    fn record_for(generation: u64, relay: &SigningKey, user: &SigningKey) -> PresenceRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let not_before = 1_700_000_000_000;
        let not_after = not_before + 600_000;

        let user_msg = presence_record_user_signing_input(&user_ipk, &relay_id, generation);
        let user_sig = user.sign(&user_msg);
        let relay_msg = presence_record_relay_signing_input(
            &user_ipk,
            &relay_id,
            &relay_pubkey,
            not_before,
            not_after,
            generation,
            0,
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
            capabilities: 0,
            user_sig: user_sig.to_bytes().into(),
            relay_sig: relay_sig.to_bytes().into(),
        }
    }

    #[test]
    fn decide_lookup_outcome_empty_replies_returns_not_present() {
        match decide_lookup_outcome(Vec::new()) {
            FindValueOutcome::NotPresent => {}
            FindValueOutcome::Found(_) => panic!("empty replies must not return Found"),
        }
    }

    #[test]
    fn decide_lookup_outcome_lone_found_with_two_not_present_returns_not_present() {
        // Eclipse case: 2 NotPresent + 1 Found from the K-closest.
        // Strict quorum (= 2) requires 2 *Found* to agree;
        // 1 alone is insufficient → NotPresent.
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let rec = record_for(5, &relay, &user);

        let replies = vec![
            ValueReply::NotPresent,
            ValueReply::NotPresent,
            ValueReply::Found(rec),
        ];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::NotPresent => {}
            FindValueOutcome::Found(_) => {
                panic!("lone Found must not pass quorum")
            }
        }
    }

    #[test]
    fn decide_lookup_outcome_two_agreeing_found_passes_quorum() {
        // 2 Found + 1 NotPresent: the two `Found` replies agree on
        // `(generation, relay_id)` — quorum reached, return that record.
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let rec = record_for(5, &relay, &user);

        let replies = vec![
            ValueReply::Found(rec.clone()),
            ValueReply::Found(rec.clone()),
            ValueReply::NotPresent,
        ];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::Found(r) => {
                assert_eq!(r.generation, rec.generation);
                assert_eq!(r.relay_id, rec.relay_id);
            }
            FindValueOutcome::NotPresent => panic!("2 agreeing Found must pass quorum"),
        }
    }

    #[test]
    fn decide_lookup_outcome_picks_higher_generation_when_two_groups_quorum() {
        // 2 Found(rec_v5) + 2 Found(rec_v7) — both groups reach quorum.
        // The canonical ordering picks the higher generation.
        let user = fresh_signing_key();
        let relay_a = fresh_signing_key();
        let relay_b = fresh_signing_key();
        let rec_v5 = record_for(5, &relay_a, &user);
        let rec_v7 = record_for(7, &relay_b, &user);

        let replies = vec![
            ValueReply::Found(rec_v5.clone()),
            ValueReply::Found(rec_v5.clone()),
            ValueReply::Found(rec_v7.clone()),
            ValueReply::Found(rec_v7.clone()),
        ];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::Found(r) => assert_eq!(r.generation, 7),
            FindValueOutcome::NotPresent => panic!("two groups at quorum should yield Found"),
        }
    }

    #[test]
    fn decide_lookup_outcome_one_each_no_quorum_returns_not_present() {
        // 1 Found + 1 NotPresent: neither path reaches quorum.
        // Returns NotPresent (the doubt-defaults-to-offline rule).
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let rec = record_for(1, &relay, &user);

        let replies = vec![ValueReply::Found(rec), ValueReply::NotPresent];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::NotPresent => {}
            FindValueOutcome::Found(_) => panic!("no group reached quorum"),
        }
    }

    #[test]
    fn decide_lookup_outcome_all_not_present_returns_not_present() {
        let replies = vec![
            ValueReply::NotPresent,
            ValueReply::NotPresent,
            ValueReply::NotPresent,
        ];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::NotPresent => {}
            FindValueOutcome::Found(_) => panic!("all NotPresent → NotPresent"),
        }
    }

    #[test]
    fn decide_lookup_outcome_two_found_diff_groups_no_quorum() {
        // 1 Found(rec_v5) + 1 Found(rec_v7): two different groups,
        // each of size 1. Neither reaches quorum. Result: NotPresent.
        let user = fresh_signing_key();
        let relay_a = fresh_signing_key();
        let relay_b = fresh_signing_key();
        let rec_v5 = record_for(5, &relay_a, &user);
        let rec_v7 = record_for(7, &relay_b, &user);

        let replies = vec![ValueReply::Found(rec_v5), ValueReply::Found(rec_v7)];
        match decide_lookup_outcome(replies) {
            FindValueOutcome::NotPresent => {}
            FindValueOutcome::Found(_) => panic!("disagreeing single Founds must not pass quorum"),
        }
    }
}
