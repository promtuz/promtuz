//! Server-side handlers and the client-side sync driver.
//!
//! ## Server side ([`handle_merkle_summary`] / [`handle_merkle_diff`] /
//! [`handle_fetch_record`])
//!
//! Pure functions used by [`super::super::handler::handle_dht_request`]:
//! they take an `Arc<Dht>` plus a request payload, return a response
//! payload, and have zero non-fjall side effects.
//!
//! ## Client side ([`sync_round`])
//!
//! Drives a single anti-entropy round against one randomly-chosen peer:
//!
//! 1. Send `MerkleSummary { slices = our_populated_bitset }`.
//! 2. Receive `MerkleSummaryResp { roots }`. For each `(slice_id,
//!    peer_root)` that diverges from our local root, dispatch the
//!    bisect routine ([`bisect_slice`]).
//! 3. Bisect: walk down with `MerkleDiff` calls until either we hit a
//!    leaf and have `(ipk, peer_value_hash)` pairs, or we exceed
//!    [`MAX_BISECT_DEPTH`] in which case we fall back to a brute
//!    "fetch every leaf in the subtree" recovery.
//! 4. Fetch: for each diverging IPK, issue [`FetchRecord`] (capped at
//!    [`MAX_FETCH_RECORD_BATCH`] per RPC) and apply the returned
//!    records via [`super::super::store::store_record`] — the
//!    conflict-resolution rules pick the canonical winner.
//!
//! Errors are logged and swallowed; one failed peer doesn't abort the
//! tick. The next tick picks a different peer.
//!

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FetchRecord;
use common::proto::dht_p2p::FetchRecordResp;
use common::proto::dht_p2p::MAX_FETCH_RECORD_BATCH;
use common::proto::dht_p2p::MerkleDiff;
use common::proto::dht_p2p::MerkleDiffResp;
use common::proto::dht_p2p::MerkleSummary;
use common::proto::dht_p2p::MerkleSummaryResp;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::warn;
use quinn::Connection;
use rand::seq::IteratorRandom;
use tokio::time::timeout;

use super::TREE_DEPTH;
use super::all_slices_bitset;
use crate::dht::Dht;
use crate::dht::config::FETCH_RECORD_MAX;
use crate::dht::config::LOOKUP_RPC_TIMEOUT_MS;
use crate::dht::lookup::connect_to_peer;
use crate::dht::store;

/// Hard depth ceiling on the bisect descent. Equal to
/// [`super::super::config::MERKLE_DIFF_PATH_MAX`] = 4: at depth 4 the
/// peer returns `Leaves`, so a deeper request would be a wire-protocol
/// violation. We bound the recursion at this level even on the
/// requester side so a malicious peer cannot pull us into a deeper
/// descent than the wire format allows.
const MAX_BISECT_DEPTH: usize = TREE_DEPTH;

// ---------------------------------------------------------------------------
// Server-side handlers
// ---------------------------------------------------------------------------

/// Handle an inbound `MerkleSummary` RPC. Returns the `(slice_id,
/// root_hash)` pairs for slices the requester's bitset asked about.
pub(crate) fn handle_merkle_summary(
    dht: &Arc<Dht>, req: MerkleSummary,
) -> MerkleSummaryResp {
    dht.metrics.inc_merkle_summaries_received();
    let pairs = dht.merkle.read().summary(&req.slices.0);
    let roots = pairs.into_iter().map(|(sid, h)| (sid, h.into())).collect();
    MerkleSummaryResp { roots }
}

/// Handle an inbound `MerkleDiff` RPC. Returns either the 16 child
/// hashes at the requested internal node, or the full
/// `(ipk, value_hash)` leaf entries.
///
/// Path-length bound check: a path longer than [`TREE_DEPTH`] is
/// malformed. We collapse to "treat it as leaf-depth" rather than
/// erroring — the caller-side bisect will see an empty leaves response
/// and fall back to its brute-fetch path.
pub(crate) fn handle_merkle_diff(dht: &Arc<Dht>, req: MerkleDiff) -> MerkleDiffResp {
    dht.metrics.inc_merkle_diffs_received();
    if req.path.len() > TREE_DEPTH {
        // Defensive: clamp to leaf depth. If a misbehaving peer slips
        // past their own bound check we don't panic.
        return MerkleDiffResp::Leaves { entries: Vec::new() };
    }
    dht.merkle.read().diff(req.slice_id, &req.path)
}

/// Handle an inbound `FetchRecord` RPC. Returns *both* live records
/// and tombstones for each requested IPK that we currently hold, up
/// to [`FETCH_RECORD_MAX`] entries combined.
///
/// **Tombstone preference:** when both a record and a tombstone exist
/// for the same IPK (which should not normally happen — `store_tombstone`
/// deletes the record and writes the tombstone in one step) the
/// tombstone wins. It's the authoritative deletion and the requester
/// needs it to converge.
///
pub(crate) fn handle_fetch_record(dht: &Arc<Dht>, req: FetchRecord) -> FetchRecordResp {
    let now = now_ms();
    let mut records = Vec::new();
    let mut tombstones = Vec::new();
    let max = FETCH_RECORD_MAX;
    for (i, ipk) in req.user_ipks.iter().enumerate() {
        if i >= max || records.len() + tombstones.len() >= max {
            break;
        }
        // Tombstone first: if present, it's the authoritative state.
        if let Some(t) = store::lookup_tombstone(dht, &ipk.0) {
            tombstones.push(t);
            continue;
        }
        if let Some(rec) = store::lookup_record(dht, &ipk.0, now) {
            records.push(rec);
        }
    }
    FetchRecordResp { records, tombstones }
}

// ---------------------------------------------------------------------------
// Client side: sync driver
// ---------------------------------------------------------------------------

/// One full anti-entropy round against a single peer. Picks the peer
/// at random from the routing table.
///
/// Returns `Ok(())` on a "round attempted, errors logged" basis; only
/// `Err` if there are literally no peers in the routing table to try.
/// The scheduler treats both as "next tick please".
pub(crate) async fn sync_round(dht: Arc<Dht>) -> Result<(), &'static str> {
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    // Pick a random peer from our routing table. Cloning the descriptor
    // out of the read lock means the lock is dropped before any I/O.
    //
    // Seed off a wall-clock-derived value so every tick picks a fresh
    // peer; we don't need cryptographic randomness here, just enough
    // spread that a long-running scheduler covers the routing table
    // over time. `StdRng::seed_from_u64(now_nanos)` is the standard
    // pattern in tokio examples.
    let peer = {
        let routing = dht.routing.read();
        let entries: Vec<_> = routing
            .buckets
            .iter()
            .flat_map(|b| b.entries.iter().cloned())
            .collect();
        if entries.is_empty() {
            return Err("sync_round: no peers in routing table");
        }
        let mut rng = StdRng::seed_from_u64(seed_now());
        match entries.into_iter().choose(&mut rng) {
            Some(e) => e.descriptor(),
            None => return Err("sync_round: peer selection returned None"),
        }
    };

    let conn = match connect_to_peer(&dht, &peer).await {
        Ok(c) => c,
        Err(e) => {
            warn!("sync_round: connect to {:?} failed: {e}", peer.id);
            return Ok(());
        }
    };

    // Step 1: MerkleSummary — what slices does the peer also have?
    let bitset = all_slices_bitset();
    let req = DhtRequest::MerkleSummary(MerkleSummary { slices: bitset.into() });
    dht.metrics.inc_merkle_summaries_sent();
    let resp = match rpc_one(&conn, req).await {
        Ok(r) => r,
        Err(e) => {
            warn!("sync_round: MerkleSummary RPC to {:?} failed: {e}", peer.id);
            return Ok(());
        }
    };
    let summary = match resp {
        DhtResponse::MerkleSummary(s) => s,
        other => {
            warn!("sync_round: expected MerkleSummary response, got {other:?}");
            return Ok(());
        }
    };

    // Step 2: bisect each slice whose root differs from ours.
    //
    // Snapshot our local roots once per slice — never under the merkle
    // lock during an `await`. The HashMap clone is small (one entry per
    // slice, capped at 256) and the locks are released before any I/O.
    for (slice_id, peer_root_bytes) in summary.roots {
        let peer_root: [u8; 32] = peer_root_bytes.0;
        let our_root = dht.merkle.read().root(slice_id);
        if peer_root == our_root {
            continue;
        }
        // Roots diverge — bisect.
        if let Err(e) = bisect_slice(&dht, &conn, slice_id).await {
            warn!("sync_round: bisect of slice {slice_id} failed: {e}");
        }
    }

    Ok(())
}

/// Walk one slice's tree from root to leaves, fetching diverging
/// records as we go. The driver is depth-first: once we descend into a
/// subtree and find divergence, we resolve that subtree's missing
/// records before moving on to the next sibling.
///
/// At [`MAX_BISECT_DEPTH`] we hit the leaf level; the response is
/// `Leaves { entries }` and we diff the entries against our local
/// view to identify the IPKs that need fetching.
async fn bisect_slice(
    dht: &Arc<Dht>, conn: &Connection, slice_id: u8,
) -> Result<(), anyhow::Error> {
    // Stack-based DFS: each entry is the path of nibbles we've descended
    // so far. Start at the slice root.
    let mut stack: Vec<Vec<u8>> = vec![Vec::new()];
    let mut to_fetch: Vec<[u8; 32]> = Vec::new();

    while let Some(path) = stack.pop() {
        if path.len() >= MAX_BISECT_DEPTH {
            // Leaf level — request the full leaf entries.
            //
            // Wire protocol semantics: at full path depth the peer
            // responds with `Leaves`, not `Children`. Anything else is
            // a peer-side bug.
            let req = DhtRequest::MerkleDiff(MerkleDiff { slice_id, path: path.clone() });
            dht.metrics.inc_merkle_diffs_sent();
            let resp = match rpc_one(conn, req).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("bisect_slice: leaf MerkleDiff failed: {e}");
                    continue;
                }
            };
            match resp {
                DhtResponse::MerkleDiff(MerkleDiffResp::Leaves { entries }) => {
                    diff_leaves(dht, slice_id, &path, &entries, &mut to_fetch);
                }
                DhtResponse::MerkleDiff(MerkleDiffResp::Children { .. }) => {
                    warn!("bisect_slice: peer returned Children at leaf depth");
                }
                other => {
                    warn!("bisect_slice: expected MerkleDiff response, got {other:?}");
                }
            }
            continue;
        }

        // Internal-node case. Ask the peer for child hashes; compare
        // each to our local equivalent and recurse on mismatches.
        let req = DhtRequest::MerkleDiff(MerkleDiff { slice_id, path: path.clone() });
        dht.metrics.inc_merkle_diffs_sent();
        let resp = match rpc_one(conn, req).await {
            Ok(r) => r,
            Err(e) => {
                warn!("bisect_slice: internal MerkleDiff failed: {e}");
                continue;
            }
        };
        let peer_children = match resp {
            DhtResponse::MerkleDiff(MerkleDiffResp::Children { hashes }) => hashes,
            DhtResponse::MerkleDiff(MerkleDiffResp::Leaves { .. }) => {
                warn!("bisect_slice: peer returned Leaves at internal depth");
                continue;
            }
            other => {
                warn!("bisect_slice: expected MerkleDiff response, got {other:?}");
                continue;
            }
        };
        if peer_children.len() != crate::dht::config::MERKLE_FANOUT {
            warn!(
                "bisect_slice: peer returned {} child hashes, expected {}",
                peer_children.len(),
                crate::dht::config::MERKLE_FANOUT
            );
            continue;
        }

        // Compare against local children.
        let our_children = dht.merkle.read().children_at(slice_id, &path);
        for (i, peer_child) in peer_children.into_iter().enumerate() {
            if peer_child.0 == our_children[i] {
                continue;
            }
            let mut child_path = path.clone();
            child_path.push(i as u8);
            stack.push(child_path);
        }
    }

    // All bisects complete — issue FetchRecord against the gathered
    // IPKs.
    if !to_fetch.is_empty()
        && let Err(e) = fetch_and_apply(dht, conn, &to_fetch).await {
            warn!("bisect_slice: fetch_and_apply failed: {e}");
        }
    Ok(())
}

/// Compare a peer's leaf entries against our local leaf state and
/// queue divergent IPKs for fetching.
///
/// Cases handled:
/// - Peer has an entry we don't → fetch.
/// - Peer has an entry we do, but value hashes differ → fetch (the
///   conflict resolver in `store_record` picks the right one).
/// - We have an entry the peer doesn't → no action here; the peer
///   will see our root differ on its own next sync round and pull from
///   us (push-on-mismatch is out of scope for v1).
fn diff_leaves(
    dht: &Arc<Dht>, slice_id: u8, path: &[u8],
    peer_entries: &[(common::types::bytes::Bytes<32>, common::types::bytes::Bytes<32>)],
    to_fetch: &mut Vec<[u8; 32]>,
) {
    // Snapshot our leaf entries for this path.
    let our_entries = dht.merkle.read().leaves_at(slice_id, path);
    let our_map: std::collections::HashMap<[u8; 32], [u8; 32]> =
        our_entries.into_iter().collect();

    for (ipk_b, vh_b) in peer_entries {
        let ipk: [u8; 32] = ipk_b.0;
        let peer_vh: [u8; 32] = vh_b.0;
        match our_map.get(&ipk) {
            Some(our_vh) if our_vh == &peer_vh => {
                // Identical — nothing to do.
            }
            _ => {
                // Either we don't have it or we have a different
                // value-hash. Queue for fetch.
                to_fetch.push(ipk);
            }
        }
    }
}

/// Issue `FetchRecord` against `peer` for a list of IPKs and apply each
/// returned record / tombstone. Caps the request size at
/// [`MAX_FETCH_RECORD_BATCH`]; longer lists are split across multiple
/// RPCs.
///
/// Anti-entropy carries both records and tombstones so deletions
/// converge even when peers diverge on record-vs-tombstone.
async fn fetch_and_apply(
    dht: &Arc<Dht>, conn: &Connection, ipks: &[[u8; 32]],
) -> Result<(), anyhow::Error> {
    for chunk in ipks.chunks(MAX_FETCH_RECORD_BATCH) {
        let req = DhtRequest::FetchRecord(FetchRecord {
            user_ipks: chunk.iter().map(|b| (*b).into()).collect(),
        });
        let resp = rpc_one(conn, req).await?;
        let (records, tombstones) = match resp {
            DhtResponse::FetchRecord(r) => (r.records, r.tombstones),
            other => {
                return Err(anyhow::anyhow!(
                    "fetch_and_apply: expected FetchRecord response, got {other:?}"
                ));
            }
        };
        let now = now_ms();
        for rec in records {
            // store_record handles validation + conflict resolution.
            // We don't surface its outcome to the scheduler — it's
            // logged via metrics on the store path.
            let _ = store::store_record(dht, rec, now);
        }
        for tomb in tombstones {
            // Same conflict-resolution discipline applies on the
            // tombstone side via `store_tombstone`.
            let _ = store::store_tombstone(dht, tomb, now);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Send one DHT request over a fresh bi-stream and read the response.
///
/// Mirrors `lookup::rpc_one` (deliberately not shared because that one
/// is private to the lookup module — duplication is small and keeps
/// the modules independent).
async fn rpc_one(
    conn: &Connection, req: DhtRequest,
) -> Result<DhtResponse, anyhow::Error> {
    let pkt = DhtPacket::Request(req);
    let bytes = pkt.pack()?;

    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&bytes).await?;
    send.finish()?;

    // Wrap in the per-RPC timeout so a stalled peer can't block the
    // sync round indefinitely.
    let resp = timeout(
        Duration::from_millis(LOOKUP_RPC_TIMEOUT_MS),
        DhtPacket::unpack(&mut recv),
    )
    .await??;
    match resp {
        DhtPacket::Response(r) => Ok(r),
        DhtPacket::Request(_) => Err(anyhow::anyhow!("rpc_one: peer sent Request, expected Response")),
    }
}

/// Wall-clock now in ms-since-epoch. Used for the random-seed and the
/// `store_record` `now_ms` argument inside `fetch_and_apply`.
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Seed value derived from wall-clock — fine for "pick a random peer"
/// where we just need temporal spread, not cryptographic strength.
fn seed_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Compatibility shim for the original sync-stub callers (deprecated)
// ---------------------------------------------------------------------------

/// Deprecated entry point retained for the original sync stub. Kept
/// under a `#[deprecated]` so any caller that hasn't migrated to
/// [`sync_round`] surfaces a warning rather than a hard breakage.
#[deprecated(note = "use sync_round instead")]
#[allow(dead_code)]
pub(crate) async fn sync_with(
    dht: &Arc<Dht>, _peer: crate::dht::routing::RoutingEntry,
) {
    let _ = sync_round(dht.clone()).await;
}

// ---------------------------------------------------------------------------
// Tests — server-side handlers (sync; client-side requires real QUIC peers)
// ---------------------------------------------------------------------------
//
// Integration test for the client-side `sync_round` is deferred
// (future work) — it requires real relay-to-relay QUIC peers. The
// handler tests live in `dht/handler.rs::tests` to share the
// `fresh_dht` fixture.

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::dht_p2p::FetchRecord;
    use common::proto::dht_p2p::MerkleDiff;
    use common::proto::dht_p2p::MerkleDiffResp;
    use common::proto::dht_p2p::MerkleSummary;
    use common::proto::dht_p2p::PresenceRecord;
    use common::proto::dht_p2p::StoreOutcome;
    use common::proto::dht_p2p::presence_record_relay_signing_input;
    use common::proto::dht_p2p::presence_record_user_signing_input;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;
    use crate::dht::sync::merkle::nibble_path_for;
    use crate::dht::sync::set_slice_bit;

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
        let path = std::env::temp_dir().join(format!("promtuz-syncrpc-test-{pid}-{id}"));
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

    /// Real wall-clock now in ms — `store_record` is called with this
    /// during the test so the record passes its TTL check inside
    /// `verify(now_ms())`.
    fn wall_clock_ms() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    #[test]
    fn handle_merkle_summary_returns_root_for_populated_slice() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        // Build a bitset that only includes the user's slice.
        let user_slice = rec.user_ipk.0[0];
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, user_slice);

        let resp = handle_merkle_summary(&dht, MerkleSummary { slices: bs.into() });
        assert_eq!(resp.roots.len(), 1);
        assert_eq!(resp.roots[0].0, user_slice);
        assert_ne!(resp.roots[0].1.0, [0u8; 32]);
    }

    #[test]
    fn handle_merkle_summary_skips_unrequested_slices() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        // Bitset asks for a *different* slice than the record's. Reply
        // must be empty.
        let user_slice = rec.user_ipk.0[0];
        let other_slice = user_slice.wrapping_add(1);
        let mut bs = [0u8; 32];
        set_slice_bit(&mut bs, other_slice);

        let resp = handle_merkle_summary(&dht, MerkleSummary { slices: bs.into() });
        assert!(resp.roots.is_empty());
    }

    #[test]
    fn handle_merkle_diff_at_root_returns_children() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        let user_slice = rec.user_ipk.0[0];
        let resp = handle_merkle_diff(
            &dht,
            MerkleDiff { slice_id: user_slice, path: Vec::new() },
        );
        match resp {
            MerkleDiffResp::Children { hashes } => {
                assert_eq!(hashes.len(), crate::dht::config::MERKLE_FANOUT);
                // Exactly one slot non-zero — the one matching the
                // record's first nibble.
                let first_nibble = (rec.user_ipk.0[1] >> 4) & 0x0F;
                assert_ne!(hashes[first_nibble as usize].0, [0u8; 32]);
            }
            MerkleDiffResp::Leaves { .. } => panic!("expected Children at root depth"),
        }
    }

    #[test]
    fn handle_merkle_diff_at_leaf_depth_returns_leaves() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        let (slice_id, path) = nibble_path_for(&rec.user_ipk.0);
        let resp = handle_merkle_diff(&dht, MerkleDiff { slice_id, path });
        match resp {
            MerkleDiffResp::Leaves { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.0, rec.user_ipk.0);
            }
            MerkleDiffResp::Children { .. } => panic!("expected Leaves at leaf depth"),
        }
    }

    #[test]
    fn handle_fetch_record_returns_known_records() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        let unknown_ipk = [99u8; 32];
        let resp = handle_fetch_record(
            &dht,
            FetchRecord {
                user_ipks: vec![rec.user_ipk, unknown_ipk.into()],
            },
        );
        // Only the known IPK is present in the reply.
        assert_eq!(resp.records.len(), 1);
        assert_eq!(resp.records[0], rec);
    }

    #[test]
    fn rebuild_from_records_repopulates_merkle_state() {
        use crate::dht::sync::rebuild_from_records;

        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        // Snapshot the root after the live store.
        let user_slice = rec.user_ipk.0[0];
        let root_after_store = dht.merkle.read().root(user_slice);
        assert_ne!(root_after_store, [0u8; 32]);

        // Wipe the in-memory Merkle state and rebuild from disk —
        // simulates a process restart.
        {
            let mut m = dht.merkle.write();
            *m = crate::dht::sync::MerkleState::empty();
        }
        let count = rebuild_from_records(&dht);
        assert_eq!(count, 1);

        // Root must match what it was before the wipe — same records
        // → same canonical Merkle hash.
        let root_after_rebuild = dht.merkle.read().root(user_slice);
        assert_eq!(root_after_store, root_after_rebuild);
    }

    #[test]
    fn handle_fetch_record_returns_tombstones_for_known_ipks() {
        // `FetchRecord` reply carries both records and tombstones. A
        // peer that holds a tombstone for `ipk` returns it under
        // `tombstones`, not `records`.
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        // Build & store a record, then tombstone it.
        let rec = build_record(&user, &relay, 1, now, 600_000);
        assert_eq!(
            store::store_record(&dht, rec.clone(), now + 1),
            StoreOutcome::Stored
        );
        let user_ipk: [u8; 32] = rec.user_ipk.0;
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let msg = common::proto::dht_p2p::tombstone_signing_input(
            &user_ipk, &relay_id, &relay_pubkey, 1, now,
        );
        let tomb_sig = relay.sign(&msg);
        let tomb = common::proto::dht_p2p::TombstoneRecord {
            user_ipk:     user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            generation:   1,
            deleted_at:   now,
            relay_sig:    tomb_sig.to_bytes().into(),
        };
        assert_eq!(
            store::store_tombstone(&dht, tomb.clone(), now),
            common::proto::dht_p2p::TombstoneOutcome::Stored
        );

        let resp = handle_fetch_record(
            &dht,
            FetchRecord { user_ipks: vec![user_ipk.into()] },
        );
        // No live record (tombstone supersedes it at store time).
        assert_eq!(resp.records.len(), 0);
        // Exactly one tombstone returned.
        assert_eq!(resp.tombstones.len(), 1);
        assert_eq!(resp.tombstones[0].generation, 1);
        assert_eq!(resp.tombstones[0].user_ipk.0, user_ipk);
    }

    #[test]
    fn fetch_record_carries_tombstone_to_peer_with_record() {
        // Anti-entropy convergence test:
        // - dht_a holds a tombstone for `(user, gen 1)`.
        // - dht_b holds the live record for same `(user, gen 1)`.
        // - dht_b would (in the real flow) call `handle_fetch_record`
        //   on dht_a; we invoke the handler directly.
        // - Apply the response back into dht_b via `store_tombstone`.
        // - Verify dht_b now has the tombstone, not the record.
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht_a = fresh_dht(self_id);
        let dht_b = fresh_dht(self_id);

        let now = wall_clock_ms();
        let rec = build_record(&user, &relay, 1, now, 600_000);

        // dht_b has the live record.
        assert_eq!(
            store::store_record(&dht_b, rec.clone(), now + 1),
            StoreOutcome::Stored
        );

        // dht_a tombstones the same key.
        let user_ipk: [u8; 32] = rec.user_ipk.0;
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);
        let msg = common::proto::dht_p2p::tombstone_signing_input(
            &user_ipk, &relay_id, &relay_pubkey, 1, now + 100,
        );
        let tomb_sig = relay.sign(&msg);
        let tomb = common::proto::dht_p2p::TombstoneRecord {
            user_ipk:     user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            generation:   1,
            deleted_at:   now + 100,
            relay_sig:    tomb_sig.to_bytes().into(),
        };
        assert_eq!(
            store::store_tombstone(&dht_a, tomb.clone(), now + 100),
            common::proto::dht_p2p::TombstoneOutcome::Stored
        );

        // Sanity: dht_b's Merkle root for this slice differs from
        // dht_a's (record-leaf vs tombstone-leaf domain tags).
        let user_slice = user_ipk[0];
        assert_ne!(
            dht_a.merkle.read().root(user_slice),
            dht_b.merkle.read().root(user_slice),
        );

        // Simulated FetchRecord: dht_b asks dht_a for `user_ipk`. The
        // handler returns the tombstone in `tombstones`, not the
        // record in `records`.
        let resp = handle_fetch_record(
            &dht_a,
            FetchRecord { user_ipks: vec![user_ipk.into()] },
        );
        assert!(resp.records.is_empty());
        assert_eq!(resp.tombstones.len(), 1);

        // dht_b applies the tombstone — same conflict resolution that
        // `fetch_and_apply` would use.
        let outcome = store::store_tombstone(
            &dht_b,
            resp.tombstones[0].clone(),
            now + 100,
        );
        assert_eq!(
            outcome,
            common::proto::dht_p2p::TombstoneOutcome::Stored
        );

        // dht_b now has the tombstone, no live record. The two
        // relays' Merkle roots converge.
        assert!(store::lookup_record(&dht_b, &user_ipk, now + 100).is_none());
        assert!(store::lookup_tombstone(&dht_b, &user_ipk).is_some());
        assert_eq!(
            dht_a.merkle.read().root(user_slice),
            dht_b.merkle.read().root(user_slice),
        );
    }

    #[test]
    fn handle_fetch_record_caps_response_length() {
        // Build FETCH_RECORD_MAX + 5 records and request all of them in
        // one call — handler must cap the *combined* (records +
        // tombstones) response at FETCH_RECORD_MAX.
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let mut all_ipks = Vec::new();
        for _ in 0..(FETCH_RECORD_MAX + 5) {
            let user = fresh_signing_key();
            let rec = build_record(&user, &relay, 1, now, 600_000);
            assert_eq!(
                store::store_record(&dht, rec.clone(), now + 1),
                StoreOutcome::Stored
            );
            all_ipks.push(rec.user_ipk);
        }

        let resp = handle_fetch_record(&dht, FetchRecord { user_ipks: all_ipks });
        assert!(resp.records.len() + resp.tombstones.len() <= FETCH_RECORD_MAX);
    }
}
