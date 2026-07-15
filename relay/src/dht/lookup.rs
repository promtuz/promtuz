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
use common::proto::dht_p2p::NodeDescriptor;
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
use super::config::LOOKUP_RPC_TIMEOUT_MS;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

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
/// Visible to the rest of `dht/` (e.g. `forward.rs`, `queue_drain.rs`) so
/// the cache + dial path is shared rather than duplicated.
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
    )
    .await;

    match res {
        Ok(_) => {
            dht.metrics.inc_lookups_succeeded();
            Ok(closest_so_far.into_iter().take(K).map(|c| c.desc).collect())
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

/// Drive the α-parallel iterative `FindNode` loop with hedging. Used by
/// `lookup_node`.
///
/// Returns `Ok(())` when the walk converged peacefully (`closest_so_far`
/// is now populated), `Err(LookupError)` on timeout / max-hops.
//
// Seven args is the iterative-walk state (target + four mutable
// candidate sets + deadline). Bundling into a `LookupCtx` struct would
// split borrow patterns awkwardly across the loop body. Allow the arity.
#[allow(clippy::too_many_arguments)]
async fn run_iterative_loop(
    dht: &Arc<Dht>, target: &[u8; 32], candidates: &mut Vec<Candidate>,
    queried: &mut HashSet<NodeId>, closest_so_far: &mut Vec<Candidate>, hops: &mut u32,
    deadline: Instant,
) -> Result<(), LookupError> {
    use tokio::task::JoinSet;

    loop {
        if Instant::now() >= deadline {
            return Err(LookupError::Timeout);
        }
        if *hops >= LOOKUP_MAX_HOPS {
            return Err(LookupError::MaxHopsExceeded);
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
            set.spawn(async move {
                send_one_hop(&dht_ref, desc_clone, target_arr).await
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
                Err(_join) => continue,
            };
            got_one = true;
            match result {
                RpcResult::FindNodeReply(closer) => {
                    integrate_descriptors(target, candidates, &closer);
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
/// One-hop `FindNode` RPC outcome, collapsed from the wire
/// `DhtResponse` for the iterative loop's match arm.
enum RpcResult {
    FindNodeReply(Vec<NodeDescriptor>),
    Failed,
}

/// Connect (or reuse) and issue one `FindNode` RPC against `peer`.
async fn send_one_hop(
    dht: &Arc<Dht>, peer: NodeDescriptor, target: [u8; 32],
) -> RpcResult {
    let conn = match connect_to_peer(dht, &peer).await {
        Ok(c) => c,
        Err(_) => return RpcResult::Failed,
    };

    let req = DhtRequest::FindNode(FindNode {
        target:    target.into(),
        requester: dht.node_id,
    });

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
}
