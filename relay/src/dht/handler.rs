//! Inbound `peer/1` connection dispatcher.
//!
//! Replaces the old `relay/src/quic/handler/peer.rs` no-op stub with a
//! single funnel into the DHT's RPC handlers. One QUIC connection ⇒ one
//! task spawned in `handle_peer_connection`; that task waits for an
//! application-layer signed handshake, then accepts bi-streams in a
//! loop and dispatches each to a per-RPC handler.
//!
//! ## Phase 1i: signed `DhtHello` first, then RPCs
//!
//! Before any RPC is accepted, the dialer must send a [`DhtHello`]
//! (Ed25519-signed transcript binding `node_id` to `pubkey` and a
//! fresh timestamp) on the **first uni-stream** of the connection.
//! The receiver:
//!
//! 1. Awaits `accept_uni()` with a 5-second timeout (
//!    [`HELLO_RECV_TIMEOUT`]). A peer that connects but never sends a
//!    hello gets dropped — see [`HELLO_RECV_TIMEOUT`] doc-comment for
//!    the rationale on the value.
//! 2. Decodes via `DhtHello::unpack` and validates with
//!    `DhtHello::verify(now_ms())` — same checks the resolver does for
//!    `RelayHello` (id-binding, signature, ±60s skew window).
//! 3. On any failure, closes with the appropriate `CloseReason::Dht*`
//!    and bumps `metrics.dht_hello_rejected`. On success, the
//!    authenticated `NodeId` is bound to the connection for its full
//!    lifetime.
//!
//! Authenticated identity then **replaces** the synthetic-stable_id
//! and `[0u8; 32]` placeholders that phase 1h had to use because the
//! TLS server config uses `with_no_client_auth()` (clients also reuse
//! the same endpoint, so we can't enable mTLS without splitting the
//! endpoint — see `relay/src/dht/tls_extract.rs` for that gap).
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
//! Every successful inbound RPC is observable as a "this peer is
//! alive" signal — we touch the routing table by calling
//! `RoutingTable::insert` with the dialer's authenticated NodeId,
//! `addr`, and verified pubkey. Phase 1i closes the placeholder-pubkey
//! gap from phase 1h: the pubkey now comes from the verified `DhtHello`
//! (`BLAKE3(pubkey) == node_id` was checked at hello time), not the
//! `[0u8; 32]` placeholder.
//!
//! ## Per-peer rate limiting
//!
//! Phase 1h item 2: every inbound RPC is also passed through the
//! per-peer keyed rate limiter on `Dht::rate_limiters` before being
//! dispatched. Phase 1i: the limiter key is now the authenticated
//! `NodeId` for *every* inbound RPC, including the ones that don't
//! carry an in-payload `requester` field (Ping, Store, Tombstone,
//! MerkleSummary, MerkleDiff, FetchRecord). A reconnecting attacker
//! therefore can't reset their quota — the NodeId is identity-bound.
//! Tripping the limiter closes the whole connection with
//! `CloseReason::DhtFlood` (and bumps `metrics.rate_limit_rejections`).
//!
//! design-doc: §2.3 (ALPN reuse: `peer/1` = relay-to-relay), §2.4 (RPC
//! catalogue), §2.5 (close-reason codes), §3.4 (peer learning from
//! inbound RPCs), §8.1-8.2 (Sybil/eclipse rationale for identity
//! binding).

use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtHelloVerifyError;
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
use tokio::time::timeout;

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

/// Maximum time the receiver waits for the dialer's first uni-stream
/// carrying a [`DhtHello`]. A peer that connects but never sends a
/// hello is dropped at this point.
///
/// 5 s is several orders of magnitude above the round-trip needed to
/// open a uni-stream and ship a 130-byte signed packet, but well below
/// the QUIC idle timeout (`common/src/quic/config.rs:32-33`, 30 s) so
/// a stalled hello doesn't get caught only by the idle path. Matches
/// the order of magnitude of `LOOKUP_RPC_TIMEOUT_MS` (1500 ms in
/// `dht/config.rs`), with extra slack for the *first* packet on a
/// freshly-handshaked connection where TLS warmup can dominate.
const HELLO_RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Drive a single inbound `peer/1` connection through its full lifetime.
///
/// 1. Best-effort TLS leaf-cert pubkey extraction via
///    [`tls_extract::extract_pubkey_from_leaf_der`]. Under the relay's
///    current `with_no_client_auth()` server config this typically
///    yields no cert chain at all; preserved as a forward-looking
///    cross-check that *if* a cert chain ever lands (e.g. once mTLS
///    is enabled on `peer/1` per the gap doc'd in `tls_extract.rs`),
///    the cert SPKI agrees with the application-layer hello below.
/// 2. **Phase 1i — application-layer signed handshake:** wait up to
///    [`HELLO_RECV_TIMEOUT`] for the dialer's first uni-stream and
///    decode it as a [`DhtHello`]. Verify with `DhtHello::verify` —
///    on any failure close the connection with the appropriate
///    `CloseReason::Dht*` and bump `metrics.dht_hello_rejected`.
///    On success the `(authenticated_id, authenticated_pubkey)` pair
///    is bound to this connection for its full lifetime, and the
///    routing table / `peer_conns` cache is populated immediately
///    so anti-entropy and bucket-refresh can find this peer even
///    before it sends any RPC.
/// 3. Wait for bi-streams in a loop.
/// 4. Spawn a per-stream task that reads one DhtRequest, checks the
///    per-peer rate limiter ([`crate::dht::rate_limit`]) keyed on the
///    *authenticated* NodeId from step 2, dispatches via
///    `handle_dht_request`, writes the matching DhtResponse, and
///    `finish()`es the send side.
/// 5. On `Connection::closed()` (peer rebooted, network failed), evict
///    the routing-table entry only if it still points at this exact
///    `Connection` — same race-guard as `remove_client_if_same` at
///    `relay/src/quic/handler/client/mod.rs:43-52`.
///
/// design-doc: §2.3, §2.5, §3.4, §7.1, §8.7 (per-peer rate limiting).
pub(crate) async fn handle_peer_connection(dht: Arc<Dht>, conn: Connection) {
    let limiter = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS_PER_PEER));
    let conn_id = conn.stable_id();

    // Forward-compatible TLS pubkey extraction. Under the current
    // `with_no_client_auth()` server config this returns `None`
    // (clients don't present certs); preserved as defense-in-depth
    // for the day mTLS lands on `peer/1`. If a cert chain *is* present
    // and cleanly parses we cross-check it against the application-
    // layer hello below: any mismatch is a hard close.
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

    // Phase 1i: wait for, decode, and verify the dialer's signed
    // `DhtHello`. The bound NodeId is the connection's authenticated
    // identity for the rest of its lifetime.
    let auth = match recv_and_verify_hello(&dht, &conn).await {
        Ok(a) => a,
        Err(()) => {
            // recv_and_verify_hello already mapped the failure to a
            // close-reason and bumped metrics; nothing more to do.
            return;
        }
    };

    // Optional cross-check: if mTLS *did* yield a cert SPKI and the
    // hello-claimed pubkey disagrees, the connection is malicious or
    // misconfigured. Same reasoning as the outbound-side post-handshake
    // check at `lookup::connect_to_peer`.
    if let Some(cert_pk) = extracted_pubkey {
        if cert_pk != auth.pubkey {
            dht.metrics.inc_dht_hello_rejected();
            common::warn!(
                "DHT inbound: cert SPKI != DhtHello.pubkey for {}; closing",
                auth.node_id
            );
            CloseReason::DhtBadSignature.close(&conn);
            return;
        }
    }

    // Populate routing-table + peer_conns cache *now*, before any RPC
    // arrives. Phase 1h had to defer this to the per-request path
    // because there was no authenticated identity at connection time;
    // phase 1i lets us do it once at the natural boundary, which also
    // means RPCs that don't carry a `requester` field (Ping, Store,
    // Tombstone, etc.) still get routing-table coverage.
    {
        let desc = NodeDescriptor {
            id:     auth.node_id,
            addr:   conn.remote_address(),
            pubkey: auth.pubkey.into(),
        };
        let _ = dht.routing.write().insert(desc);
    }
    {
        let mut map = dht.peer_conns.write();
        // Race: another task may have raced ahead via outbound dial
        // (`lookup::connect_to_peer`) to the same peer; drop the loser.
        // We arbitrarily keep the first-cached entry so reconnection
        // storms don't churn the cached `Connection` across a workload.
        map.entry(auth.node_id).or_insert_with(|| (conn.clone(), auth.pubkey));
    }
    dht.metrics.inc_dht_hello_accepted();

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
        let auth_clone = auth;
        tokio::spawn(async move {
            let _permit = permit;
            let mut recv = recv;
            let send = send;
            handle_one_stream(dht_clone, conn_for_task, send, &mut recv, auth_clone).await;
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

/// Per-connection authenticated identity established by the [`DhtHello`]
/// handshake. Once set, never re-verified for the connection's lifetime —
/// the QUIC connection itself binds the peer to its peer-identifier
/// (`stable_id`), and rotating the bound NodeId mid-connection has no
/// known threat-model justification.
///
/// `Copy` because every field is plain bytes; cheap to pass-by-value
/// into the per-stream task.
#[derive(Clone, Copy, Debug)]
struct AuthenticatedPeer {
    node_id: NodeId,
    pubkey:  [u8; 32],
}

/// Read the dialer's first uni-stream, decode as [`DhtHello`], verify,
/// and on success return the authenticated `(node_id, pubkey)` pair.
///
/// Failure modes are exhaustively mapped to `CloseReason::Dht*`
/// variants per design-doc §2.5:
///
/// | Cause | CloseReason |
/// |---|---|
/// | No uni-stream within `HELLO_RECV_TIMEOUT` | `DhtClockSkew` (re-used: peer "missed its window") |
/// | Connection died before any frame | (connection already closed; no close-reason) |
/// | Frame failed to decode as `DhtHello` | `DhtMalformedKey` |
/// | `DhtHello::verify`: `IdMismatch` or `MalformedPubkey` | `DhtMalformedKey` |
/// | `DhtHello::verify`: `BadSignature` | `DhtBadSignature` |
/// | `DhtHello::verify`: `ClockSkew` | `DhtClockSkew` |
///
/// All paths bump `metrics.dht_hello_rejected` exactly once on failure,
/// `metrics.dht_hello_accepted` once on success.
async fn recv_and_verify_hello(
    dht: &Arc<Dht>, conn: &Connection,
) -> Result<AuthenticatedPeer, ()> {
    // Wait for the first uni-stream within HELLO_RECV_TIMEOUT.
    let mut recv = match timeout(HELLO_RECV_TIMEOUT, conn.accept_uni()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            // Connection died before any frame arrived. The connection
            // is already closed — nothing for us to close. We still
            // bump the rejection counter so operators see this in
            // metrics rather than diagnosing it via QUIC logs.
            dht.metrics.inc_dht_hello_rejected();
            common::debug!(
                "DHT inbound from {}: connection ended before DhtHello: {e}",
                conn.remote_address()
            );
            return Err(());
        }
        Err(_) => {
            // Timeout. Peer connected but never sent a hello.
            dht.metrics.inc_dht_hello_rejected();
            common::warn!(
                "DHT inbound from {}: no DhtHello within {:?}; closing",
                conn.remote_address(),
                HELLO_RECV_TIMEOUT
            );
            CloseReason::DhtClockSkew.close(conn);
            return Err(());
        }
    };

    // Decode the framed DhtHello. `unpack` reads the u16 length prefix
    // and the body, applying `DhtHello`'s `Deserialize` impl.
    let hello: DhtHello = match DhtHello::unpack(&mut recv).await {
        Ok(h) => h,
        Err(e) => {
            dht.metrics.inc_dht_hello_rejected();
            common::warn!(
                "DHT inbound from {}: malformed DhtHello frame: {e}",
                conn.remote_address()
            );
            CloseReason::DhtMalformedKey.close(conn);
            return Err(());
        }
    };

    // Verify (id-binding, pubkey shape, signature, timestamp window).
    let now = now_ms();
    match verify_hello_with_close_reason(&hello, now) {
        Ok(()) => Ok(AuthenticatedPeer {
            node_id: hello.node_id,
            pubkey:  hello.pubkey.0,
        }),
        Err(reason) => {
            dht.metrics.inc_dht_hello_rejected();
            common::warn!(
                "DHT inbound from {} failed DhtHello verification; closing with {:?}",
                conn.remote_address(),
                reason
            );
            reason.close(conn);
            Err(())
        }
    }
}

/// Pure helper: verify `hello` against `now_ms`, mapping any
/// [`DhtHelloVerifyError`] to the `CloseReason::Dht*` we'd send on the
/// wire. Extracted from [`recv_and_verify_hello`] so the close-reason
/// mapping can be unit-tested without spinning up QUIC.
///
/// The mapping table — same as the doc on
/// [`recv_and_verify_hello`] — is:
///
/// | Verify error | CloseReason |
/// |---|---|
/// | `IdMismatch` / `MalformedPubkey` | `DhtMalformedKey` |
/// | `BadSignature` | `DhtBadSignature` |
/// | `ClockSkew` | `DhtClockSkew` |
fn verify_hello_with_close_reason(
    hello: &DhtHello, now_ms: u64,
) -> Result<(), CloseReason> {
    hello.verify(now_ms).map_err(|e| match e {
        DhtHelloVerifyError::IdMismatch | DhtHelloVerifyError::MalformedPubkey => {
            CloseReason::DhtMalformedKey
        }
        DhtHelloVerifyError::BadSignature => CloseReason::DhtBadSignature,
        DhtHelloVerifyError::ClockSkew => CloseReason::DhtClockSkew,
    })
}

/// Read one request frame, dispatch, write one response frame.
///
/// `auth` is the connection-bound [`AuthenticatedPeer`] established by
/// `recv_and_verify_hello` at connection accept time. **Every** stream
/// on the connection — regardless of which RPC it carries — keys the
/// rate limiter and refreshes the routing table against this same
/// authenticated NodeId. This closes the phase 1h gap where RPCs
/// without an in-payload `requester` (Ping, Store, Tombstone,
/// MerkleSummary, MerkleDiff, FetchRecord) fell back to a per-
/// connection synthetic id and a `[0u8; 32]` placeholder pubkey.
///
/// Phase 1h item 2 (still applies): per-peer rate-limit check happens
/// **after** the request is fully parsed — parse-then-check is the
/// safer pattern because a malformed wire payload also gets caught
/// here (parse failure → `DhtMalformedKey` close), and a misbehaving
/// peer can't avoid the bookkeeping cost of one parse per RPC.
async fn handle_one_stream(
    dht: Arc<Dht>, conn: Connection, mut send: SendStream,
    recv: &mut quinn::RecvStream, auth: AuthenticatedPeer,
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

    // Phase 1i: per-peer inbound rate limiting now keyed on the
    // authenticated NodeId for *every* RPC kind. A reconnecting
    // attacker cannot reset their quota — the NodeId is identity-
    // bound by the signed `DhtHello` we admitted at connection time.
    let class = RpcClass::for_request(&req);
    if dht.rate_limiters.check(&auth.node_id, class).is_err() {
        dht.metrics.inc_rate_limit_rejections();
        common::warn!(
            "DHT inbound rate limit tripped (peer={}, class={class:?}); closing connection",
            auth.node_id
        );
        CloseReason::DhtFlood.close(&conn);
        return;
    }

    let resp = handle_dht_request(&dht, req).await;

    // Routing-table feedback: refresh the peer's last-seen status.
    // Insertion already happened at connection accept time; this
    // is the LRU-rotate-to-tail path inside `RoutingTable::insert`.
    {
        let desc = NodeDescriptor {
            id:     auth.node_id,
            addr:   conn.remote_address(),
            pubkey: auth.pubkey.into(),
        };
        // Scoped write guard, never held across `await`.
        let _ = dht.routing.write().insert(desc);
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
        // ----- Sticky-home phase 2a: wire-type stubs --------------------
        //
        // Phase 2a (this commit) lands only the wire-format contract; the
        // real handlers ship in phase 2d (`STICKY_HOME_RELAY.md` §8 row
        // 2d). Until then, return defensive "we declined" outcomes so
        // an early adopter that speaks the new variant doesn't deadlock
        // on a missing dispatcher arm — they will see `NotOwner` /
        // empty-batch / `ok=false` and back off cleanly.
        //
        // The metrics counters / actual delivery path land in 2d; here
        // we deliberately do not increment any DHT metric so phase 2d's
        // observability story can be designed wholesale.
        DhtRequest::Forward(_) => {
            DhtResponse::Forward(common::proto::dht_p2p::ForwardResp {
                outcome: common::proto::dht_p2p::ForwardOutcome::NotOwner,
            })
        }
        DhtRequest::QueueFetch(_) => {
            DhtResponse::QueueFetch(common::proto::dht_p2p::QueueFetchResp {
                messages:  Vec::new(),
                exhausted: true,
            })
        }
        DhtRequest::QueueFetchAck(_) => {
            DhtResponse::QueueFetchAck(common::proto::dht_p2p::QueueFetchAckResp {
                ok: false,
            })
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

    // ---------------------------------------------------------------
    // Phase 1i — DhtHello close-reason mapping
    // ---------------------------------------------------------------

    use common::proto::dht_p2p::DhtHello;
    use common::proto::dht_p2p::dht_hello_signing_input;
    use common::types::bytes::Bytes;

    /// Build a freshly-signed `DhtHello` for `key` at `timestamp`.
    /// Mirrors the production dialer in `lookup::send_dht_hello`.
    fn make_hello(key: &SigningKey, timestamp: u64) -> DhtHello {
        let pubkey: [u8; 32] = key.verifying_key().to_bytes();
        let node_id = NodeId::new(pubkey);
        let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp);
        let sig = key.sign(&msg).to_bytes();
        DhtHello {
            node_id,
            pubkey: Bytes(pubkey),
            timestamp,
            sig: Bytes(sig),
        }
    }

    #[test]
    fn verify_hello_close_reason_maps_clock_skew() {
        // Stale timestamp → CloseReason::DhtClockSkew.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let stale = make_hello(&key, now - 120_000); // 2 min in the past
        match verify_hello_with_close_reason(&stale, now) {
            Err(CloseReason::DhtClockSkew) => {}
            other => panic!("expected DhtClockSkew, got {other:?}"),
        }
    }

    #[test]
    fn verify_hello_close_reason_maps_bad_signature() {
        // Tamper signature → CloseReason::DhtBadSignature.
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let mut hello = make_hello(&key, now);
        hello.sig.0[0] ^= 0xFF;
        match verify_hello_with_close_reason(&hello, now) {
            Err(CloseReason::DhtBadSignature) => {}
            other => panic!("expected DhtBadSignature, got {other:?}"),
        }
    }

    #[test]
    fn verify_hello_close_reason_maps_id_mismatch_to_malformed_key() {
        // A pubkey that doesn't hash to the claimed node_id surfaces
        // as `DhtMalformedKey` per the mapping table — same close
        // bucket as a malformed-Ed25519-key shape.
        let key_a = fresh_signing_key();
        let key_b = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let mut hello = make_hello(&key_a, now);
        // Replace node_id with a different identity's id while keeping
        // the original (a-derived) pubkey + sig.
        hello.node_id = NodeId::new(key_b.verifying_key().to_bytes());
        match verify_hello_with_close_reason(&hello, now) {
            Err(CloseReason::DhtMalformedKey) => {}
            other => panic!("expected DhtMalformedKey, got {other:?}"),
        }
    }

    #[test]
    fn verify_hello_close_reason_passes_freshly_signed() {
        let key = fresh_signing_key();
        let now: u64 = 1_700_000_000_000;
        let hello = make_hello(&key, now);
        verify_hello_with_close_reason(&hello, now).expect("valid hello must pass");
        verify_hello_with_close_reason(&hello, now + 5)
            .expect("inside skew window must pass");
    }

    #[test]
    fn dht_hello_metrics_initially_zero_then_bump_on_reject() {
        // Tests the metrics-counter wiring for the dht_hello_*
        // counters added in phase 1i. Drive the counters via the
        // public increment helpers (the same helpers
        // `recv_and_verify_hello` calls) and confirm the observed
        // values change predictably.
        let mut self_seed = [0u8; 32];
        self_seed[0] = 1;
        let self_id = NodeId::new(self_seed);
        let dht = fresh_dht(self_id);

        // Initial state.
        assert_eq!(
            dht.metrics
                .dht_hello_accepted
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            dht.metrics
                .dht_hello_rejected
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );

        // Bump and observe.
        dht.metrics.inc_dht_hello_accepted();
        dht.metrics.inc_dht_hello_rejected();
        dht.metrics.inc_dht_hello_rejected();
        assert_eq!(
            dht.metrics
                .dht_hello_accepted
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            dht.metrics
                .dht_hello_rejected
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }
}
