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
//! Phase 1h plumbs the cert-chain pubkey through when available
//! (`tls_extract::extract_pubkey_from_leaf_der`); when the relay's
//! `peer/1` server config is `with_no_client_auth()` (current
//! production state), `peer_identity()` returns `None` and we use a
//! `[0u8; 32]` placeholder. The cert-pin check is enforced on the
//! *outbound* side (`lookup::connect_to_peer`); see `tls_extract.rs`
//! for the inbound-mTLS gap.
//!
//! ## Per-peer rate limiting
//!
//! Phase 1h item 2: every inbound RPC is also passed through the
//! per-peer keyed rate limiter on `Dht::rate_limiters` before being
//! dispatched. Tripping the limiter closes the whole connection with
//! `CloseReason::DhtFlood` (and bumps `metrics.rate_limit_rejections`).
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
use super::rate_limit::RpcClass;
use super::routing::RoutingTable;
use super::store;
use super::tls_extract;

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
/// 1. Attempt to extract the peer's TLS leaf-cert pubkey via
///    [`tls_extract::extract_pubkey_from_leaf_der`]. Under the relay's
///    current `with_no_client_auth()` server config this returns
///    `Err(NoCertChain)` because the dialing peer never presents a
///    client cert. We log once at info-level and proceed with a
///    `[0u8; 32]` placeholder pubkey for any routing-table inserts.
///    See module docs in `tls_extract.rs` for the inbound-mTLS gap.
/// 2. Wait for bi-streams in a loop.
/// 3. Spawn a per-stream task that reads one DhtRequest, checks the
///    per-peer rate limiter ([`crate::dht::rate_limit`]), dispatches via
///    `handle_dht_request`, writes the matching DhtResponse, and
///    `finish()`es the send side.
/// 4. On `Connection::closed()` (peer rebooted, network failed), evict
///    the routing-table entry only if it still points at this exact
///    `Connection` — same race-guard as `remove_client_if_same` at
///    `relay/src/quic/handler/client/mod.rs:43-52`.
///
/// design-doc: §2.3, §3.4, §7.1, §8.7 (per-peer rate limiting).
pub(crate) async fn handle_peer_connection(dht: Arc<Dht>, conn: Connection) {
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS_PER_PEER));
    let conn_id = conn.stable_id();

    // Phase 1h item 1: best-effort TLS pubkey extraction. Under the
    // current relay TLS server config (`with_no_client_auth`), the
    // dialer does not present a client cert and `peer_identity()`
    // returns `None` — `tls_extract::*` surfaces that as
    // `Err(NoCertChain)`. We absorb the error and use the placeholder
    // pubkey for routing-table inserts; the cert-pin check is enforced
    // at the *outbound* path via `lookup::connect_to_peer`. Documented
    // gap; closing it requires switching `peer/1` to mTLS.
    //
    // If the relay later flips to mTLS, the same code path here will
    // start succeeding and back-fill verified pubkeys without further
    // changes.
    let extracted_pubkey: Option<[u8; 32]> = {
        match conn.peer_identity().and_then(|id| {
            id.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                .and_then(|chain| chain.first().cloned())
        }) {
            Some(leaf) => match tls_extract::extract_pubkey_from_leaf_der(leaf.as_ref()) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    dht.metrics.inc_cert_pubkey_extraction_failures();
                    common::warn!(
                        "DHT inbound peer connection: cert chain present but pubkey extraction failed: {e}"
                    );
                    CloseReason::DhtMalformedKey.close(&conn);
                    return;
                }
            },
            None => None,
        }
    };

    loop {
        let stream = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break, // connection closed or errored
        };
        let (send, recv) = stream;

        let permit = match limiter.clone().try_acquire_owned() {
            Ok(p) => p,
            // Peer over-streamed; close the new stream politely and
            // continue the accept loop. The per-RPC-kind rate limits
            // applied inside `handle_one_stream` are the second-stage
            // defence; this concurrency cap is a coarse first-line
            // bulkhead.
            Err(_) => continue,
        };

        let dht_clone = dht.clone();
        let conn_for_task = conn.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut recv = recv;
            let send = send;
            handle_one_stream(
                dht_clone,
                conn_for_task,
                send,
                &mut recv,
                extracted_pubkey,
            )
            .await;
        });
    }

    // Connection closed — evict routing-table entry if still ours.
    let peer_id_to_remove: Option<NodeId> = {
        let map = dht.peer_conns.read();
        map.iter().find_map(|(id, (c, _pk))| {
            if c.stable_id() == conn_id {
                Some(*id)
            } else {
                None
            }
        })
    };
    if let Some(id) = peer_id_to_remove {
        let mut map = dht.peer_conns.write();
        if let Some((c, _pk)) = map.get(&id) {
            if c.stable_id() == conn_id {
                map.remove(&id);
                dht.metrics.inc_peer_conns_closed();
            }
        }
    }
}

/// Read one request frame, dispatch, write one response frame.
///
/// `cert_pubkey` is the pubkey extracted (or stubbed as `None`) by
/// [`handle_peer_connection`]. When present we use it as the routing-
/// table entry's pubkey; absent (the inbound-no-mTLS case) we fall
/// back to `[0u8; 32]`.
///
/// Phase 1h item 2: per-peer rate-limit check happens **after** the
/// request is fully parsed — parse-then-check is the safer pattern
/// because a malformed wire payload also gets caught here (parse
/// failure → `DhtMalformedKey` close), and a misbehaving peer can't
/// avoid the bookkeeping cost of one parse per RPC. The downside is
/// minimal: we still spent O(few KB) of postcard decode before
/// deciding to throttle.
async fn handle_one_stream(
    dht: Arc<Dht>, conn: Connection, mut send: SendStream,
    recv: &mut quinn::RecvStream, cert_pubkey: Option<[u8; 32]>,
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

    let requester_id = requester_from_request(&req);

    // Per-peer inbound rate limiting (phase 1h item 2). Keying:
    // - If the RPC carries a `requester` NodeId (FindNode, FindValue),
    //   key on that — it's the cleanest cross-connection identity.
    // - Otherwise, key on a *synthetic* NodeId derived from the QUIC
    //   `stable_id()` of this connection. This is per-connection
    //   (not per-peer-NodeId) and so a single misbehaving peer that
    //   reconnects gets a fresh quota — but the per-source-IP rate
    //   limiter at the acceptor (a follow-up; see report) is the
    //   right place to bound that. Within one connection, the
    //   synthetic NodeId still bounds a flood of `Store`s or
    //   `FetchRecord`s, which is the immediate DoS vector this item
    //   targets.
    //
    // The cert-pubkey-derived NodeId would be a stronger key (it's
    // what cert-pinning protects). Today the relay's `peer/1`
    // server-side TLS config uses `with_no_client_auth()` so we
    // never see the dialer's cert; cached `cert_pubkey` is `None`.
    // When mTLS lands, swap in `cert_pubkey` here.
    let limiter_key = match (requester_id, cert_pubkey) {
        (Some(id), _) => id,
        (None, Some(pk)) => NodeId::new(pk),
        (None, None) => synthetic_id_from_conn(&conn),
    };
    let class = RpcClass::for_request(&req);
    if dht.rate_limiters.check(&limiter_key, class).is_err() {
        dht.metrics.inc_rate_limit_rejections();
        common::warn!(
            "DHT inbound rate limit tripped (key={limiter_key}, class={class:?}); closing connection"
        );
        CloseReason::DhtFlood.close(&conn);
        return;
    }

    let resp = handle_dht_request(&dht, req).await;

    // Routing-table feedback: insert or refresh the requester. Use the
    // post-handshake-extracted cert pubkey when available (item 1);
    // otherwise the placeholder. Even the placeholder path is useful —
    // the requester's `id` and `addr` populate the routing table so
    // later outbound dials can verify the cert chain via
    // `lookup::connect_to_peer`'s post-handshake check.
    if let Some(id) = requester_id {
        let pubkey = cert_pubkey.unwrap_or([0u8; 32]);
        let desc = NodeDescriptor {
            id,
            addr:   conn.remote_address(),
            pubkey: pubkey.into(),
        };
        // Scoped write guard, never held across `await`.
        let outcome = {
            let mut routing = dht.routing.write();
            routing.insert(desc)
        };
        let _ = outcome;
    }

    // Cache the connection. The cached pubkey here is whatever the
    // post-handshake extraction returned (often `[0u8; 32]` on the
    // inbound side under `with_no_client_auth`).
    if let Some(id) = requester_id {
        let mut map = dht.peer_conns.write();
        map.entry(id).or_insert_with(|| (conn.clone(), cert_pubkey.unwrap_or([0u8; 32])));
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
/// RPC kinds that don't carry one (`Ping`, `Store`, `Tombstone`,
/// `MerkleSummary`, `MerkleDiff`, `FetchRecord`).
fn requester_from_request(req: &DhtRequest) -> Option<NodeId> {
    match req {
        DhtRequest::FindNode(r) => Some(r.requester),
        DhtRequest::FindValue(r) => Some(r.requester),
        // Anonymous in the wire form. Phase 1h's rate limiter falls
        // back to a synthetic per-connection key for these.
        _ => None,
    }
}

/// Derive a synthetic [`NodeId`] from a QUIC connection's
/// `stable_id()`. Used by phase 1h's rate limiter as a per-connection
/// fallback key when the inbound RPC carries no `requester` NodeId
/// and the relay's TLS server config doesn't yield a peer cert.
///
/// `BLAKE3(stable_id_le_bytes)` is enough — a single connection has
/// one `stable_id` for its lifetime and we just need a stable-per-call
/// key for the keyed rate limiter, not a meaningful identity. Using a
/// cryptographic hash (rather than a raw integer) keeps `NodeId` space
/// uniform and avoids collisions with any real peer's NodeId.
fn synthetic_id_from_conn(conn: &Connection) -> NodeId {
    let bytes = (conn.stable_id() as u64).to_le_bytes();
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&bytes);
    NodeId::new(seed)
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

    /// Phase 1h item 2 — rate-limit wiring: drive the per-peer
    /// limiter through the same primitive the handler uses, against
    /// a fresh `Dht`. This is the integration-equivalent of
    /// `rate_limit::tests::limiter_grants_burst_then_denies` but
    /// exercises the actual `Dht::rate_limiters` field (so an
    /// accidental refactor that builds a fresh limiter per call
    /// would surface here as "no rate limit ever trips").
    #[tokio::test(flavor = "current_thread")]
    async fn handle_dispatch_per_peer_rate_limit_trips_on_store_burst() {
        use crate::dht::config::RATE_LIMIT_EXPENSIVE_BURST;
        use crate::dht::rate_limit::RpcClass;

        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        let peer_id = NodeId::new([0xAA; 32]);

        // Drain the burst.
        for _ in 0..((RATE_LIMIT_EXPENSIVE_BURST as usize) + 5) {
            let _ = dht.rate_limiters.check(&peer_id, RpcClass::Expensive);
        }

        // Subsequent rapid checks should now trip — the burst is
        // exhausted and the steady-state rate hasn't refilled.
        let mut denied = 0;
        for _ in 0..50 {
            if dht.rate_limiters.check(&peer_id, RpcClass::Expensive).is_err() {
                denied += 1;
            }
        }
        assert!(denied > 0, "Dht::rate_limiters must trip under burst");
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
