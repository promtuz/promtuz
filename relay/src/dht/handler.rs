//! Inbound `peer/1` connection dispatcher.
//!
//! Replaces the old `relay/src/quic/handler/peer.rs` no-op stub with a
//! single funnel into the DHT's RPC handlers. One QUIC connection ⇒ one
//! task spawned in `handle_peer_connection`; that task accepts bi-streams
//! in a loop and dispatches each to a per-RPC handler.
//!
//! ## Per-stream dispatch
//!
//! Per design-doc §2.2, every DHT RPC is one bi-stream: open_bi → write
//! request → finish() send → read response → done. The acceptor side
//! mirrors that: accept_bi → read request → write response → finish.
//!
//! ## Concurrency cap
//!
//! Per-peer concurrent in-flight RPC streams are capped via a
//! `tokio::sync::Semaphore` (the same idiom as `client/mod.rs`'s
//! 16-stream limiter). Phase 1h hardens this further with per-RPC-kind
//! rate limits.
//!
//! ## Routing-table feedback
//!
//! Every successful inbound RPC is observable as a "the requester is
//! alive" signal — we touch the routing table by calling
//! `RoutingTable::insert` with the requester's NodeId / addr / pubkey.
//! Phase 1h plumbs the cert-chain pubkey through; phase 1d uses
//! `[0u8; 32]` placeholder. **(Marked TODO in code.)**
//!
//! design-doc: §2.3 (ALPN reuse: `peer/1` = relay-to-relay), §2.4 (RPC
//! catalogue), §3.4 (peer learning from inbound RPCs).

use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FindNodeResp;
use common::proto::dht_p2p::FindValueOutcome as WireFindValueOutcome;
use common::proto::dht_p2p::FindValueResp;
use common::proto::dht_p2p::MAX_FIND_NODE_RESULTS;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::Pong;
use common::proto::dht_p2p::StoreResp;
use common::proto::dht_p2p::TombstoneResp;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use common::quic::id::NodeId;
use quinn::Connection;
use quinn::SendStream;
use tokio::sync::Semaphore;

use super::Dht;
use super::routing::RoutingTable;
use super::store;

/// Maximum concurrent in-flight inbound DHT streams per peer connection.
///
/// 16 matches the existing per-client limiter at
/// `relay/src/quic/handler/client/mod.rs:77`. Past this, additional
/// streams are dropped at `try_acquire_owned` rather than queued — the
/// peer is misbehaving (DHT RPCs are bounded by §2.6 length limits and
/// shouldn't pile up).
///
/// design-doc: §8.7 (DoS / floods).
const MAX_CONCURRENT_STREAMS_PER_PEER: usize = 16;

/// Drive a single inbound `peer/1` connection through its full lifetime.
///
/// 1. Wait for bi-streams in a loop.
/// 2. Spawn a per-stream task that reads one DhtRequest, dispatches via
///    `handle_dht_request`, writes the matching DhtResponse, and
///    `finish()`es the send side.
/// 3. On `Connection::closed()` (peer rebooted, network failed), evict
///    the routing-table entry only if it still points at this exact
///    `Connection` — same race-guard as `remove_client_if_same` at
///    `relay/src/quic/handler/client/mod.rs:43-52`.
///
/// design-doc: §2.3, §3.4, §7.1.
pub(crate) async fn handle_peer_connection(dht: Arc<Dht>, conn: Connection) {
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS_PER_PEER));
    let conn_id = conn.stable_id();

    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break, // connection closed or errored
        };
        let (send, recv) = stream;

        let permit = match limiter.clone().try_acquire_owned() {
            Ok(p) => p,
            // Peer over-streamed; close the new stream politely and
            // continue the accept loop. Phase 1h tightens this further
            // by enforcing per-RPC-kind rate limits.
            Err(_) => continue,
        };

        let dht_clone = dht.clone();
        let conn_for_task = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // Single RPC per stream (§2.2). Read one request, write one
            // response, finish the send side.
            let mut recv = recv;
            let send = send;
            handle_one_stream(dht_clone, conn_for_task, send, &mut recv).await;
        });
    }

    // Connection closed — evict routing-table entry if still ours.
    // We don't know the peer's NodeId from this side without parsing the
    // cert chain (TODO phase 1h), but `peer_conns` is keyed by NodeId so
    // we walk it cheaply for an `Arc<Connection>`-equal match. Same
    // pattern as `remove_client_if_same`.
    let peer_id_to_remove: Option<NodeId> = {
        let map = dht.peer_conns.read();
        map.iter().find_map(|(id, c)| {
            if c.stable_id() == conn_id {
                Some(*id)
            } else {
                None
            }
        })
    };
    if let Some(id) = peer_id_to_remove {
        let mut map = dht.peer_conns.write();
        if let Some(c) = map.get(&id) {
            if c.stable_id() == conn_id {
                map.remove(&id);
                dht.metrics.inc_peer_conns_closed();
            }
        }
    }
}

/// Read one request frame, dispatch, write one response frame.
async fn handle_one_stream(
    dht: Arc<Dht>, conn: Connection, mut send: SendStream,
    recv: &mut quinn::RecvStream,
) {
    // Read request packet.
    let pkt = match DhtPacket::unpack(recv).await {
        Ok(p) => p,
        Err(_) => {
            CloseReason::DhtMalformedKey.close(&conn);
            return;
        }
    };
    let req = match pkt {
        DhtPacket::Request(r) => r,
        // A client side sending a Response on this stream is a protocol
        // violation — close.
        DhtPacket::Response(_) => {
            CloseReason::PacketMismatch.close(&conn);
            return;
        }
    };

    // Pull out the requester's NodeId before we move `req`. Used by the
    // routing-table touch below (§3.4 path 1).
    let requester_id = requester_from_request(&req);

    let resp = handle_dht_request(&dht, req).await;

    // Routing-table feedback: insert or refresh the requester. The
    // pubkey is unknown without parsing the QUIC cert chain (TODO phase
    // 1h: extract via x509-parser like resolver does in
    // `register_relay`); for now stub `[0u8; 32]`. The address comes
    // from the connection.
    if let Some(id) = requester_id {
        let desc = NodeDescriptor {
            id,
            addr:   conn.remote_address(),
            pubkey: [0u8; 32].into(), // TODO(phase 1h)
        };
        // Scoped write guard, never held across `await`.
        let outcome = {
            let mut routing = dht.routing.write();
            routing.insert(desc)
        };
        // Phase 1g: when `outcome == PendingPing(lru)`, push `lru` onto a
        // pending-pings channel so the lookup module can probe it. For
        // v1 we let routing-table churn be passive — see the dispatch
        // notes in `misc/specs/DHT.md` §3.3.
        let _ = outcome;
    }

    // Cache the connection so future outbound RPCs reuse it. The
    // routing-table-level `Weak<Connection>` field is populated by the
    // bootstrap path; the peer_conns map is the authoritative cache for
    // outbound dialing.
    if let Some(id) = requester_id {
        let mut map = dht.peer_conns.write();
        map.entry(id).or_insert_with(|| conn.clone());
    }

    // Write response.
    let bytes = match DhtPacket::Response(resp).pack() {
        Ok(b) => b,
        Err(_) => {
            CloseReason::DhtMalformedKey.close(&conn);
            return;
        }
    };
    if send.write_all(&bytes).await.is_err() {
        return;
    }
    let _ = send.finish();
}

/// Pull the requester's NodeId out of a `DhtRequest`. Returns `None` for
/// RPC kinds that don't carry one (currently `MerkleSummary` /
/// `MerkleDiff` / `FetchRecord` — phase 1g territory).
fn requester_from_request(req: &DhtRequest) -> Option<NodeId> {
    match req {
        DhtRequest::FindNode(r) => Some(r.requester),
        DhtRequest::FindValue(r) => Some(r.requester),
        // Pings/Stores/Tombstones are anonymous in the wire form; we can't
        // associate them with a NodeId without the cert chain. Phase 1h
        // backfills this once the pubkey-extraction story lands.
        _ => None,
    }
}

/// Dispatch one fully-decoded `DhtRequest` to its handler. Lives as a
/// pure function (no streams, no I/O) so unit tests can call it
/// directly.
pub(crate) async fn handle_dht_request(dht: &Arc<Dht>, req: DhtRequest) -> DhtResponse {
    match req {
        DhtRequest::Ping(p) => {
            dht.metrics.inc_pings_received();
            DhtResponse::Pong(Pong {
                nonce:     p.nonce,
                timestamp: now_ms(),
            })
        }
        DhtRequest::FindNode(f) => {
            dht.metrics.inc_find_node_rpcs();
            let target_id = NodeId::from_bytes(f.target.0);
            let closer = closest_excluding(&dht.routing.read(), &target_id, &f.requester);
            DhtResponse::FindNode(FindNodeResp { closer })
        }
        DhtRequest::FindValue(f) => {
            dht.metrics.inc_find_value_rpcs();
            let user_ipk = f.user_ipk.0;

            // First: do we have the record locally?
            let result = if let Some(record) = store::lookup_record(dht, &user_ipk, now_ms()) {
                WireFindValueOutcome::Found(record)
            } else {
                // No record. Per §4.2, we return `Closer` only if we are
                // *not* in the k closest; otherwise we return
                // `NotPresent` so the iterator can terminate. The check
                // is the same one `store_record` uses to decide
                // ownership.
                let target_id = NodeId::from_bytes(user_ipk);
                if self_in_top_k(dht, &target_id) {
                    WireFindValueOutcome::NotPresent
                } else {
                    let closer =
                        closest_excluding(&dht.routing.read(), &target_id, &f.requester);
                    WireFindValueOutcome::Closer(closer)
                }
            };
            DhtResponse::FindValue(FindValueResp { result })
        }
        DhtRequest::Store(s) => {
            let outcome = store::store_record(dht, s.record, now_ms());
            DhtResponse::Store(StoreResp { outcome })
        }
        DhtRequest::Tombstone(t) => {
            let outcome = store::store_tombstone(dht, t.record, now_ms());
            DhtResponse::Tombstone(TombstoneResp { outcome })
        }
        // Phase 1g: real anti-entropy / sync handlers.
        DhtRequest::MerkleSummary(s) => {
            DhtResponse::MerkleSummary(super::sync::rpc::handle_merkle_summary(dht, s))
        }
        DhtRequest::MerkleDiff(d) => {
            DhtResponse::MerkleDiff(super::sync::rpc::handle_merkle_diff(dht, d))
        }
        DhtRequest::FetchRecord(f) => {
            DhtResponse::FetchRecord(super::sync::rpc::handle_fetch_record(dht, f))
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wall-clock now in ms-since-Unix-epoch. Uses the same idiom as
/// `relay/src/util/mod.rs::systime` but inlined here so the handler
/// doesn't drag in a `crate::util` dependency for a one-liner.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Top-(MAX_FIND_NODE_RESULTS) descriptors closest to `target`, **excluding**
/// the `exclude` peer. Excluding the requester saves them from receiving
/// their own descriptor back, which they already know about.
fn closest_excluding(
    routing: &RoutingTable, target: &NodeId, exclude: &NodeId,
) -> Vec<NodeDescriptor> {
    routing
        .find_closest(target, MAX_FIND_NODE_RESULTS + 1)
        .into_iter()
        .filter(|d| &d.id != exclude)
        .take(MAX_FIND_NODE_RESULTS)
        .collect()
}

/// True iff `dht.self_id` would be in the top-K for `target` under the
/// current routing table. Mirrors the helper in `store.rs::self_is_owner`
/// but reads-only (no mutation, no lock-up).
fn self_in_top_k(dht: &Dht, target: &NodeId) -> bool {
    let candidates = dht.routing.read().find_closest(target, super::config::K + 1);
    if candidates.len() < super::config::K {
        return true; // be permissive while routing table is sparse
    }
    let target_bytes = target.as_bytes();
    let self_bytes = dht.node_id.as_bytes();
    let mut self_dist = [0u8; 32];
    for i in 0..32 {
        self_dist[i] = self_bytes[i] ^ target_bytes[i];
    }
    let kth = candidates[super::config::K - 1].id;
    let kth_bytes = kth.as_bytes();
    let mut kth_dist = [0u8; 32];
    for i in 0..32 {
        kth_dist[i] = kth_bytes[i] ^ target_bytes[i];
    }
    self_dist <= kth_dist
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering as AtomicOrdering;

    use common::proto::dht_p2p::DhtRequest;
    use common::proto::dht_p2p::DhtResponse;
    use common::proto::dht_p2p::FindNode;
    use common::proto::dht_p2p::FindValue;
    use common::proto::dht_p2p::Ping;
    use common::proto::dht_p2p::PresenceRecord;
    use common::proto::dht_p2p::Store;
    use common::proto::dht_p2p::StoreOutcome;
    use common::proto::dht_p2p::Tombstone;
    use common::proto::dht_p2p::TombstoneOutcome;
    use common::proto::dht_p2p::TombstoneRecord;
    use common::proto::dht_p2p::presence_record_relay_signing_input;
    use common::proto::dht_p2p::presence_record_user_signing_input;
    use common::proto::dht_p2p::tombstone_signing_input;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::dht::Dht;
    use crate::dht::DhtConfig;
    use crate::dht::dht_cf_descriptors;

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
        let path = std::env::temp_dir().join(format!("promtuz-handler-test-{pid}-{id}"));
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

    fn build_tombstone(
        user: &SigningKey, relay: &SigningKey, generation: u64, deleted_at: u64,
    ) -> TombstoneRecord {
        let user_ipk: [u8; 32] = user.verifying_key().to_bytes();
        let relay_pubkey: [u8; 32] = relay.verifying_key().to_bytes();
        let relay_id = NodeId::new(relay_pubkey);

        let msg =
            tombstone_signing_input(&user_ipk, &relay_id, &relay_pubkey, generation, deleted_at);
        let sig = relay.sign(&msg);

        TombstoneRecord {
            user_ipk: user_ipk.into(),
            relay_id,
            relay_pubkey: relay_pubkey.into(),
            generation,
            deleted_at,
            relay_sig: sig.to_bytes().into(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_ping_returns_pong_with_same_nonce() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let nonce = [42u8; 16];
        let req = DhtRequest::Ping(Ping { nonce: nonce.into(), timestamp: 999 });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::Pong(p) => {
                assert_eq!(p.nonce.0, nonce);
                // timestamp echoed from the responder; must be > the
                // request's by at most a minute or so. We just check
                // it's non-zero (clocks are real).
                assert!(p.timestamp > 0);
            }
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_find_node_returns_closer_excluding_requester() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        // Insert a few peers so the routing table has something to return.
        for n in 2..=6u8 {
            let mut seed = [0u8; 32];
            seed[0] = n;
            let id = NodeId::new(seed);
            let desc = NodeDescriptor {
                id,
                addr: "127.0.0.1:1".parse().unwrap(),
                pubkey: [0u8; 32].into(),
            };
            dht.routing.write().insert(desc);
        }

        let mut requester_seed = [0u8; 32];
        requester_seed[0] = 3;
        let requester = NodeId::new(requester_seed);
        let mut target_seed = [0u8; 32];
        target_seed[0] = 4;
        let target = NodeId::new(target_seed);

        let req = DhtRequest::FindNode(FindNode {
            target:    (*target.as_bytes()).into(),
            requester,
        });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::FindNode(r) => {
                assert!(r.closer.len() <= MAX_FIND_NODE_RESULTS);
                // Requester must be filtered out.
                assert!(r.closer.iter().all(|d| d.id != requester));
            }
            other => panic!("expected FindNode, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_find_value_returns_found_when_record_present() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        // Use the real wall-clock so `handle_dht_request`'s
        // `lookup_record(now_ms())` finds the record fresh.
        let now = wall_clock_ms();
        let record = build_record(&user, &relay, 1, now, 600_000);

        // Persist the record so FindValue should hit on it.
        let outcome = store::store_record(&dht, record.clone(), now + 1);
        assert_eq!(outcome, StoreOutcome::Stored);

        let mut requester_seed = [0u8; 32];
        requester_seed[0] = 99;
        let requester = NodeId::new(requester_seed);

        let req = DhtRequest::FindValue(FindValue {
            user_ipk: record.user_ipk,
            requester,
        });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::FindValue(r) => match r.result {
                WireFindValueOutcome::Found(rec) => assert_eq!(rec, record),
                other => panic!("expected Found, got {other:?}"),
            },
            other => panic!("expected FindValue, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_find_value_returns_not_present_when_self_in_owners() {
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        // Empty routing table → self_in_top_k returns true (permissive).
        let mut requester_seed = [0u8; 32];
        requester_seed[0] = 99;
        let requester = NodeId::new(requester_seed);

        let req = DhtRequest::FindValue(FindValue {
            user_ipk:  [7u8; 32].into(),
            requester,
        });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::FindValue(r) => assert!(matches!(r.result, WireFindValueOutcome::NotPresent)),
            other => panic!("expected FindValue, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_store_persists_valid_record() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        // Real wall-clock so the record is in-window when
        // `handle_dht_request` calls `verify(now_ms())`.
        let now = wall_clock_ms();
        let record = build_record(&user, &relay, 1, now, 600_000);

        let req = DhtRequest::Store(Store { record: record.clone() });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::Store(r) => assert_eq!(r.outcome, StoreOutcome::Stored),
            other => panic!("expected Store, got {other:?}"),
        }

        // Verify persistence — calling lookup_record should now return.
        assert!(store::lookup_record(&dht, &record.user_ipk.0, now + 1).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_tombstone_removes_existing_record() {
        let user = fresh_signing_key();
        let relay = fresh_signing_key();
        let self_id = NodeId::new(relay.verifying_key().to_bytes());
        let dht = fresh_dht(self_id);

        let now = wall_clock_ms();
        let record = build_record(&user, &relay, 5, now, 600_000);
        store::store_record(&dht, record.clone(), now + 1);

        let tomb = build_tombstone(&user, &relay, 5, now + 100);
        let req = DhtRequest::Tombstone(Tombstone { record: tomb });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::Tombstone(r) => assert_eq!(r.outcome, TombstoneOutcome::Stored),
            other => panic!("expected Tombstone, got {other:?}"),
        }

        // Record gone.
        assert!(store::lookup_record(&dht, &record.user_ipk.0, now + 100).is_none());
    }

    /// Real wall-clock now in ms. Tests that exercise
    /// `handle_dht_request` need a `not_before`/`not_after` that bracket
    /// "actual now" because the dispatcher calls `verify(now_ms())`
    /// internally.
    fn wall_clock_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_merkle_summary_with_zero_bitset_returns_no_roots() {
        // Empty bitset = "I'm interested in no slices" → empty reply
        // even on a populated relay. Mirrors the pre-phase-1g
        // placeholder behaviour for the empty-bitset case so any
        // existing peer that asks with a zero bitset gets the same
        // shape of answer.
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let req = DhtRequest::MerkleSummary(common::proto::dht_p2p::MerkleSummary {
            slices: [0u8; 32].into(),
        });
        let resp = handle_dht_request(&dht, req).await;
        match resp {
            DhtResponse::MerkleSummary(r) => assert!(r.roots.is_empty()),
            other => panic!("expected MerkleSummary, got {other:?}"),
        }
    }
}
