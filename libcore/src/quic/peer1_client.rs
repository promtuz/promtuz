//! Production `peer/1`-dialing implementation of [`DhtClient`].
//!
//! Replaces Phase 4's [`crate::quic::dht_client::NotWiredDhtClient`]
//! with a real dialer that opens libcore→relay `peer/1` connections and
//! fans out the K=3 KeyPackage / Welcome RPCs against the recipient's
//! sticky-home replica set.
//!
//! # Architecture (Phase 5a, MLS.md §11.3)
//!
//! ## Direct dial (option A)
//!
//! libcore opens its own `peer/1` connections — same ALPN the relay's
//! `dht::lookup::connect_to_peer` uses to dial peer relays. The
//! alternative considered (and rejected) was routing the RPCs through
//! the existing `relay/1` connection via a new `CRelayPacket::DhtForward`
//! variant; that doubles the relay protocol surface and complicates
//! the home's auth posture (it would have to vouch for libcore's RPCs
//! in addition to its own DHT ops). Direct dial keeps the relay
//! oblivious to libcore's DHT activity beyond what's already in-band
//! via signed payloads.
//!
//! ## Ephemeral DhtHello identity (privacy choice (b))
//!
//! Every dial generates a **fresh Ed25519 keypair** and derives
//! `node_id = BLAKE3(spki(ephemeral_pubkey))`. The relay accepts this
//! at face value (it just appends an "unknown" peer to its routing
//! table for the connection's lifetime). The relay never learns the
//! libcore user's IPK from the DhtHello — user identity surfaces only
//! via signed payloads inside the requests (`KeyPackagePublishReq.ipk`,
//! `WelcomeFetchReq.user_ipk`, etc.).
//!
//! Trade-off: the relay's routing table sees ephemeral peers
//! short-lived. We accept the mild churn because (i) per-IP rate
//! limits already cap inbound DhtHellos, (ii) bucket-eviction is LRU
//! anyway, (iii) ephemeral peers don't respond to inbound queries.
//!
//! Privacy posture caveat: libcore's TLS sub-key (cert SPKI) is
//! deterministically derived from the user's IPK via HKDF
//! (`peer_config.rs::IdentitySigningKey`). Under the current relay
//! `with_no_client_auth()` server config the cert is **never sent**
//! (rustls only ships client certs when the server requests them), so
//! cross-dial correlation via TLS sub-key is not exposed. This is a
//! soft guarantee tied to that server-config choice; if the relay ever
//! switches to `with_client_cert_verifier` the TLS layer would leak
//! the deterministic sub-key and the ephemeral-DhtHello posture would
//! degrade. Documented for §11.3.
//!
//! ## Connection pool
//!
//! Bounded LRU cache of `NodeId → (Connection, last_used: Instant,
//! ephemeral_signer: SigningKey)`. Cap = `DHT_POOL_MAX = 16`; idle
//! eviction at `DHT_CONN_IDLE_TTL = 300s`. The ephemeral signer is
//! held alongside the connection so re-dials of the same NodeId
//! don't accidentally rotate the DhtHello identity mid-flight (the
//! relay-side `peer_conns` map keys on the authenticated NodeId).
//! When a pool entry is evicted, both connection and signer drop;
//! a subsequent dial mints a fresh signer.
//!
//! ## FindNode caching
//!
//! Repeat lookups of the same key (e.g. batch sends to the same
//! recipient) reuse the cached K-closest result for `FINDNODE_CACHE_TTL =
//! 30s`. Smaller TTL than the DHT's anti-entropy cadence so a recently-
//! roamed user's K-set converges within one cache window.
//!
//! ## RPC dispatch
//!
//! - **Publish-side** (`publish_keypackages`, `refill_keypackages`,
//!   `publish_welcome_to_homes`, `ack_welcomes`): K=3 fan-out, accept
//!   on quorum (`K_MIN = 2`). Each home is dialed and the RPC is sent
//!   sequentially per home (no per-home parallelism — the K-quorum
//!   bar is low and serial dials simplify error handling); the call
//!   returns `Ok` once 2 successes are observed or `QuorumNotMet`
//!   after all 3 are tried.
//! - **Fetch-side** (`fetch_keypackage_for`, `fetch_welcomes`): try
//!   each of the K=3 in turn until one succeeds. The §5.4 cross-replica
//!   static-fields check is **NOT** wired in Phase 5a (out of scope —
//!   single-fetch semantics today; α=3 hedging deferred to Phase 5b).
//!
//! design-doc: `misc/specs/MLS.md` §3.4-3.6, §5.4, §6.1, §11.3.

#![allow(dead_code)] // Phase 5a wires this into server.rs; some helpers
// (e.g. find_k_closest result accessors) live ahead of their first
// caller.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtPacket;
use common::proto::dht_p2p::DhtRequest;
use common::proto::dht_p2p::DhtResponse;
use common::proto::dht_p2p::FindNode;
use common::proto::dht_p2p::FindNodeResp;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::dht_p2p::dht_hello_signing_input;
use common::proto::mls_wire::KP_STASH_TARGET;
use common::proto::mls_wire::KeyPackageFetchOutcome;
use common::proto::mls_wire::KeyPackageFetchReq;
use common::proto::mls_wire::KeyPackagePublishOutcome;
use common::proto::mls_wire::KeyPackagePublishReq;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::KeyPackageRefillOutcome;
use common::proto::mls_wire::KeyPackageRefillReq;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WelcomeAckReq;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::WelcomeFetchOutcome;
use common::proto::mls_wire::WelcomeFetchReq;
use common::proto::mls_wire::WelcomePublishOutcome;
use common::proto::mls_wire::WelcomePublishReq;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_refill_signing_input;
use common::proto::mls_wire::welcome_ack_signing_input;
use common::proto::mls_wire::welcome_fetch_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use parking_lot::Mutex;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;

use super::dht_client::DhtClient;
use super::dht_client::DhtClientError;
use super::dht_client::DhtClientResult;
use super::dht_client::FetchedKeyPackage;
use super::dht_client::KpOutcomeFilter;
use super::dht_client::PublishOutcome;

// ---------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------

/// Maximum entries in the per-relay connection pool. Sized for a typical
/// device that talks to its 3 K-homes plus a handful of others
/// (recipient homes for Welcome publishes, KP-fetch targets); 16 is
/// generous enough for normal use without becoming a memory footprint
/// concern.
pub const DHT_POOL_MAX: usize = 16;

/// Idle TTL for pooled connections. After 5 minutes of no use, we
/// evict and re-dial on the next call. Matches the relay's own
/// `peer_conns` lifetime expectation under typical idle traffic.
pub const DHT_CONN_IDLE_TTL: Duration = Duration::from_secs(300);

/// FindNode result cache TTL. The DHT's anti-entropy cadence is on
/// the order of 30s (§0 `ANTI_ENTROPY_INTERVAL_MS = 30s`); matching
/// that here means a stale-but-cached K-set never lags more than one
/// anti-entropy cycle behind the home's view.
pub const FINDNODE_CACHE_TTL: Duration = Duration::from_secs(30);

/// K-quorum minimum for fan-out publishes. Mirrors
/// `relay/src/dht/forward.rs`'s `K_MIN = 2` (sticky-home spec §4).
pub const K_MIN: usize = 2;

/// Per-RPC bi-stream budget (request send + response read). Matches the
/// relay's `LOOKUP_RPC_TIMEOUT_MS = 1500ms` order-of-magnitude with
/// extra slack for libcore's typical mobile-network conditions.
pub const RPC_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial-dial budget — opens the QUIC connection and sends DhtHello.
/// Larger than [`RPC_TIMEOUT`] because TLS handshake + DhtHello on a
/// freshly-cold path can dominate.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------
// Pool entry
// ---------------------------------------------------------------------

/// One entry in the per-relay connection pool. Holds the live
/// `quinn::Connection` plus the **ephemeral** Ed25519 signing key whose
/// pubkey hashed to the `node_id` in our DhtHello on this connection.
/// Re-using the same connection for multiple RPCs is fine; rotating the
/// DhtHello identity mid-connection would confuse the relay's routing
/// table, so we hold the signer for the connection's lifetime.
struct PoolEntry {
    conn:      Connection,
    last_used: Instant,
    /// Held so a future re-hello path (none today) signs under the
    /// same node identity as the original DhtHello on this connection.
    /// Drops alongside the connection on eviction → fresh signer on
    /// next dial → fresh DhtHello node_id.
    #[allow(dead_code)]
    ephemeral: Arc<SigningKey>,
}

/// **Phase 8 (P1 #16)**: collapsed-mutex pool state. Holds both the
/// connection table and the LRU order under a single `Mutex` so the
/// "lock pool then pool_order" discipline is a type-system property
/// rather than a code-review one. Invariants (always upheld inside
/// `Pool`'s methods):
///   - `entries.len() == order.len()`,
///   - `entries.contains_key(k) iff order.contains(k)`.
struct Pool {
    entries: HashMap<NodeId, PoolEntry>,
    /// LRU eviction order — front is least-recently-used, back is most.
    order:   VecDeque<NodeId>,
}

impl Pool {
    fn new() -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new() }
    }
}

// ---------------------------------------------------------------------
// FindNode cache entry
// ---------------------------------------------------------------------

#[derive(Clone)]
struct FindNodeCacheEntry {
    descriptors: Vec<NodeDescriptor>,
    cached_at:   Instant,
}

// ---------------------------------------------------------------------
// HomeDescriptor — the libcore's own home's connection coordinates.
// ---------------------------------------------------------------------

/// Connection coordinates for the user's sticky-home relay.
///
/// libcore knows the home's `(NodeId, address)` from
/// [`crate::data::relay::Relay`] (set at connect time in
/// `quic/server.rs`). The pubkey is **not** known a priori — the
/// `relay/1` ALPN dial uses CA-trust verification, so libcore never
/// learned the home relay's NodeKey pubkey. For peer/1 dials we set
/// `pubkey = None` and rely on the relay-side `with_no_client_auth()`
/// posture (no SPKI cross-check). Future protocol revisions can wire
/// this in once `Relay::refresh` starts persisting the descriptor's
/// pubkey field.
#[derive(Clone, Debug)]
pub struct HomeDescriptor {
    pub node_id: NodeId,
    pub addr:    SocketAddr,
    pub pubkey:  Option<[u8; 32]>,
}

// ---------------------------------------------------------------------
// Peer1DhtClient
// ---------------------------------------------------------------------

/// Production [`DhtClient`] dialing libcore→relay over the `peer/1`
/// ALPN.
///
/// **Construction**: callers pass:
/// - `endpoint` — the libcore-global `quinn::Endpoint` (also serves
///   `relay/1` and `client/1` outbound traffic; ALPN distinguishes).
/// - `peer_client_cfg` — `quinn::ClientConfig` configured with
///   `peer/1` ALPN. Built via [`crate::quic::peer_config::build_peer_client_cfg`]
///   from a [`crate::quic::peer_identity::PeerIdentity`].
/// - `home` — the sticky-home descriptor for routing-table seed
///   ([`fetch_welcomes`] / [`ack_welcomes`] only consult homes of *our*
///   IPK, which we always start by `FindNode`-ing through the home).
/// - `our_ipk` — the libcore user's IPK; bound into the K-set lookup
///   key for the user's own welcome queue and into the per-record
///   `owner_sig` on publishes (re-checked by callers; we don't
///   re-sign).
///
/// **Lifetime**: an `Arc<Peer1DhtClient>` is shared across the
/// scheduler tokio task, the welcome-poll-on-reconnect task, and any
/// in-flight `send_message_inner` call. All locking is `parking_lot`,
/// project-discipline; never held across `await`.
pub struct Peer1DhtClient {
    endpoint:        Endpoint,
    peer_client_cfg: Arc<ClientConfig>,
    /// **Phase 7 (P0-2)**: TLS sub-key used to build per-dial pinned
    /// `ClientConfig`s (each one wires in a [`PinnedPeerServerCertVerifier`]
    /// for the expected relay pubkey). When `None`, dials fall back to
    /// the un-pinned `peer_client_cfg` — only the test surface
    /// constructs the dialer without a sub-key today.
    tls_subkey:      Option<SigningKey>,
    home:            HomeDescriptor,
    our_ipk:         [u8; 32],
    our_ipk_signer:  SigningKey,
    /// Pool of established peer/1 connections. Phase 8 (P1 #16):
    /// `entries` and `order` were previously two separate `Mutex`es
    /// with a careful "always lock pool then pool_order" discipline;
    /// collapsed to one mutex so the lock-order invariant becomes a
    /// type-system property instead of a code-review burden. The
    /// internal `Pool` struct's invariant is `entries.len() == order.len()`
    /// and `entries.contains_key(k) <=> order.contains(k)`.
    pool:            Mutex<Pool>,
    /// FindNode lookups cached by 32-byte key (`BLAKE3("kp:" || ipk)` or
    /// `BLAKE3("welcome:" || ipk)`).
    findnode_cache:  Mutex<HashMap<[u8; 32], FindNodeCacheEntry>>,
    /// **Phase 7 (P0-4)**: integration-test observability — number of
    /// `peer/1` dials this dialer has performed (i.e. cache misses
    /// that hit `dial_and_hello`). Exposed via [`Self::dials`]. Free
    /// in production; tests assert this is non-zero after a
    /// JNI-equivalent send/receive flow to prove the production wire
    /// path was taken.
    dials_total:     std::sync::atomic::AtomicU64,
}

impl Peer1DhtClient {
    pub fn new(
        endpoint: Endpoint, peer_client_cfg: Arc<ClientConfig>, home: HomeDescriptor,
        our_ipk: [u8; 32], our_ipk_signer: SigningKey,
    ) -> Self {
        Self {
            endpoint,
            peer_client_cfg,
            tls_subkey: None,
            home,
            our_ipk,
            our_ipk_signer,
            pool: Mutex::new(Pool::new()),
            findnode_cache: Mutex::new(HashMap::new()),
            dials_total: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// **Phase 7 (P0-4)**: total `peer/1` dials performed (each one
    /// represents a cache-miss path through `dial_and_hello` — the
    /// load-bearing bit that proves we actually went over the wire).
    pub fn dials(&self) -> u64 {
        self.dials_total.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Construct an `Arc<Self>` directly (sugar for the call site, which
    /// always wraps in `Arc` for cross-task sharing).
    pub fn new_arc(
        endpoint: Endpoint, peer_client_cfg: Arc<ClientConfig>, home: HomeDescriptor,
        our_ipk: [u8; 32], our_ipk_signer: SigningKey,
    ) -> Arc<Self> {
        Arc::new(Self::new(endpoint, peer_client_cfg, home, our_ipk, our_ipk_signer))
    }

    /// **Phase 7 (P0-2)**: production-flavoured constructor that holds
    /// the TLS sub-key in addition to the pre-built un-pinned
    /// `ClientConfig`. With the sub-key on hand, every dial that knows
    /// its target's pubkey (resolver-vended via `RelayDescriptor`) can
    /// build a pinned `ClientConfig` on the fly. Production
    /// (`server.rs::build_peer1_dht_client`) uses this; tests use the
    /// legacy `new_arc` to keep the un-pinned posture explicit.
    pub fn new_arc_with_tls_subkey(
        endpoint: Endpoint, peer_client_cfg: Arc<ClientConfig>, tls_subkey: SigningKey,
        home: HomeDescriptor, our_ipk: [u8; 32], our_ipk_signer: SigningKey,
    ) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            peer_client_cfg,
            tls_subkey: Some(tls_subkey),
            home,
            our_ipk,
            our_ipk_signer,
            pool: Mutex::new(Pool::new()),
            findnode_cache: Mutex::new(HashMap::new()),
            dials_total: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Build a per-dial `ClientConfig` that pins the server SPKI to
    /// `expected`. Requires the TLS sub-key (`tls_subkey`); errors if
    /// the dialer was built without one.
    fn build_pinned_cfg(&self, expected: [u8; 32]) -> anyhow::Result<ClientConfig> {
        use crate::quic::peer_config::build_pinned_peer_client_cfg_with_subkey;
        let subkey = self
            .tls_subkey
            .as_ref()
            .ok_or_else(|| anyhow!("Peer1DhtClient: tls_subkey not set; pinning unsupported"))?;
        build_pinned_peer_client_cfg_with_subkey(subkey.clone(), expected)
    }

    // -----------------------------------------------------------------
    // Pool management
    // -----------------------------------------------------------------

    /// Returns a usable connection to `peer`, dialing if necessary.
    ///
    /// Steps:
    /// 1. Look up the pool — if a non-evicted entry exists with a live
    ///    connection (`close_reason().is_none()`), bump LRU + return.
    /// 2. Else dial: TLS handshake, send signed DhtHello with a fresh
    ///    ephemeral key, insert into the pool (evicting LRU if needed).
    ///
    /// **Phase 7 (P0-2)**: `expected_pubkey`, when `Some`, enables a
    /// per-dial TLS verifier that pins the relay's cert SPKI to the
    /// vended pubkey (resolver-authenticated via `RelayDescriptor`).
    /// `None` falls back to the un-pinned verifier — the legacy
    /// posture, kept for backward compatibility while operators
    /// migrate to NodeKey-as-cert-SPKI on the relay side.
    ///
    /// The lock ladder: pool / pool_order are taken under
    /// [`parking_lot::Mutex`], dropped before any `await`. The dial
    /// itself happens lock-free; on completion we re-take the locks to
    /// insert. Race: a parallel dial to the same peer may have
    /// inserted while we awaited; we drop our newly-dialed connection
    /// in favour of the cached one (eventual consistency on which
    /// connection future calls reuse).
    async fn get_or_dial(
        &self, peer_node_id: NodeId, addr: SocketAddr,
        expected_pubkey: Option<[u8; 32]>,
    ) -> Result<Connection, DhtClientError> {
        // Fast path: cached + alive.
        let cached = {
            let mut pool = self.pool.lock();
            Self::evict_expired_inner(&mut pool);
            // Split-borrow trick: take an Option<Connection> first via
            // get_mut, drop the borrow, then touch the LRU (which would
            // otherwise conflict with the entries borrow).
            let conn_opt = match pool.entries.get_mut(&peer_node_id) {
                Some(entry) => {
                    if entry.conn.close_reason().is_none() {
                        entry.last_used = Instant::now();
                        Some(entry.conn.clone())
                    } else {
                        None
                    }
                }
                None => None,
            };
            match conn_opt {
                Some(conn) => {
                    Self::touch_lru(&mut pool.order, peer_node_id);
                    Some(conn)
                }
                None => {
                    if pool.entries.contains_key(&peer_node_id) {
                        pool.entries.remove(&peer_node_id);
                        pool.order.retain(|id| id != &peer_node_id);
                    }
                    None
                }
            }
        };
        if let Some(conn) = cached {
            return Ok(conn);
        }

        // Dial.
        let dial_fut = self.dial_and_hello(peer_node_id, addr, expected_pubkey);
        let dialed = tokio::time::timeout(DIAL_TIMEOUT, dial_fut)
            .await
            .map_err(|_| DhtClientError::Transport(format!(
                "dial {peer_node_id} timed out after {:?}", DIAL_TIMEOUT
            )))??;

        // Insert into pool (with eviction).
        let conn = {
            let mut pool = self.pool.lock();
            // Race: another task may have populated the same key while we
            // were dialing. Prefer the existing entry to keep a stable
            // routing-table identity at the relay (the loser's ephemeral
            // key would otherwise live as an orphan k-bucket entry).
            //
            // Split-borrow: do a `get_mut` peek into a temporary
            // `Option<Connection>`, drop the entries borrow, then
            // touch LRU.
            let existing_conn = match pool.entries.get_mut(&peer_node_id) {
                Some(entry) => {
                    if entry.conn.close_reason().is_none() {
                        entry.last_used = Instant::now();
                        Some(entry.conn.clone())
                    } else {
                        None
                    }
                }
                None => None,
            };
            if let Some(c) = existing_conn {
                Self::touch_lru(&mut pool.order, peer_node_id);
                return Ok(c);
            }
            if pool.entries.contains_key(&peer_node_id) {
                pool.entries.remove(&peer_node_id);
                pool.order.retain(|id| id != &peer_node_id);
            }
            // LRU evict if at capacity.
            if pool.entries.len() >= DHT_POOL_MAX
                && let Some(oldest) = pool.order.pop_front()
            {
                pool.entries.remove(&oldest);
            }
            let conn = dialed.conn.clone();
            pool.entries.insert(peer_node_id, dialed);
            pool.order.push_back(peer_node_id);
            conn
        };
        Ok(conn)
    }

    /// Move `peer_node_id` to the back (most-recently-used) of the LRU.
    fn touch_lru(order: &mut VecDeque<NodeId>, peer_node_id: NodeId) {
        order.retain(|id| id != &peer_node_id);
        order.push_back(peer_node_id);
    }

    /// Evict any pool entries whose `last_used + DHT_CONN_IDLE_TTL` has
    /// elapsed. Caller holds the pool lock.
    fn evict_expired_inner(pool: &mut Pool) {
        let now = Instant::now();
        let expired: Vec<NodeId> = pool
            .entries
            .iter()
            .filter(|(_, e)| now.duration_since(e.last_used) > DHT_CONN_IDLE_TTL)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            pool.entries.remove(&id);
            pool.order.retain(|i| i != &id);
        }
    }

    /// Public test hook: count pool entries (no eviction). Used by
    /// the `connection_pool_*` tests.
    #[cfg(test)]
    pub(crate) fn pool_size(&self) -> usize {
        self.pool.lock().entries.len()
    }

    /// Public test hook: force eviction sweep with a custom "now"
    /// pseudo-time-skip semantics. We can't manipulate `Instant` itself,
    /// so the test instead aged the entries via direct mutation through
    /// [`Self::test_age_pool_entries`].
    #[cfg(test)]
    pub(crate) fn test_evict_expired(&self) {
        let mut pool = self.pool.lock();
        Self::evict_expired_inner(&mut pool);
    }

    /// Test-only: age every pool entry's `last_used` so the next
    /// `evict_expired` call drops them.
    #[cfg(test)]
    pub(crate) fn test_age_pool_entries(&self, age: Duration) {
        let mut pool = self.pool.lock();
        for entry in pool.entries.values_mut() {
            entry.last_used -= age;
        }
    }

    /// Test-only: snapshot the cached node_ids for ordered LRU
    /// inspection.
    #[cfg(test)]
    pub(crate) fn test_pool_order(&self) -> Vec<NodeId> {
        self.pool.lock().order.iter().copied().collect()
    }

    // -----------------------------------------------------------------
    // Dial path
    // -----------------------------------------------------------------

    /// Open the QUIC connection, send a fresh signed DhtHello, return
    /// the connection paired with the ephemeral signer that produced
    /// the DhtHello.
    ///
    /// **Phase 7 (P0-2)**: when `expected_pubkey` is `Some`, the dial
    /// uses a per-call ClientConfig with the
    /// [`PinnedPeerServerCertVerifier`] wired in — the handshake
    /// rejects any cert whose SPKI doesn't match. Otherwise the
    /// pre-built `peer_client_cfg` (un-pinned legacy verifier) is used.
    async fn dial_and_hello(
        &self, peer_node_id: NodeId, addr: SocketAddr,
        expected_pubkey: Option<[u8; 32]>,
    ) -> Result<PoolEntry, DhtClientError> {
        // 1. Open QUIC connection. SNI is the peer's NodeId in base32 —
        //    matches relay/src/dht/lookup.rs::connect_to_peer's
        //    convention. With pinning enabled the SNI is no longer the
        //    only thing keeping the dial honest; the cert SPKI is
        //    explicitly cross-checked against the resolver-vended
        //    relay pubkey.
        let sni = peer_node_id.to_string();
        let cfg = match expected_pubkey {
            Some(expected) => self
                .build_pinned_cfg(expected)
                .map_err(|e| DhtClientError::Transport(format!("pinned cfg: {e}")))?,
            None => self.peer_client_cfg.as_ref().clone(),
        };
        let conn = self
            .endpoint
            .connect_with(cfg, addr, &sni)
            .map_err(|e| DhtClientError::Transport(format!("connect setup: {e}")))?
            .await
            .map_err(|e| DhtClientError::Transport(format!("handshake: {e}")))?;

        // 2. Generate fresh ephemeral key and send DhtHello.
        let ephemeral = generate_ephemeral_signer();
        send_dht_hello(&conn, &ephemeral).await.map_err(|e| {
            DhtClientError::Transport(format!("send_dht_hello: {e}"))
        })?;

        // Phase 7 (P0-4) integration-test observability: count this
        // dial. Cheap atomic increment; not on a hot path.
        self.dials_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(PoolEntry {
            conn,
            last_used: Instant::now(),
            ephemeral: Arc::new(ephemeral),
        })
    }

    // -----------------------------------------------------------------
    // RPC helper — single bi-stream round-trip
    // -----------------------------------------------------------------

    /// Send one [`DhtRequest`] on a fresh bi-stream of `conn`, await
    /// the matching [`DhtResponse`]. Wraps the round-trip in
    /// [`RPC_TIMEOUT`].
    async fn rpc_one(
        conn: &Connection, req: DhtRequest,
    ) -> Result<DhtResponse, DhtClientError> {
        let pkt = DhtPacket::Request(req);
        let bytes = pkt
            .pack()
            .map_err(|e| DhtClientError::Protocol(format!("pack request: {e}")))?;

        let fut = async {
            let (mut send, mut recv) = conn
                .open_bi()
                .await
                .map_err(|e| DhtClientError::Transport(format!("open_bi: {e}")))?;
            send.write_all(&bytes)
                .await
                .map_err(|e| DhtClientError::Transport(format!("write request: {e}")))?;
            send.finish()
                .map_err(|e| DhtClientError::Transport(format!("finish send: {e}")))?;
            let resp_pkt = DhtPacket::unpack(&mut recv).await.map_err(|e| {
                DhtClientError::Transport(format!("unpack response: {e}"))
            })?;
            match resp_pkt {
                DhtPacket::Response(r) => Ok(r),
                DhtPacket::Request(_) => Err(DhtClientError::Protocol(
                    "peer sent a Request where Response was expected".into(),
                )),
            }
        };
        tokio::time::timeout(RPC_TIMEOUT, fut).await.map_err(|_| {
            DhtClientError::Transport(format!("rpc timed out after {:?}", RPC_TIMEOUT))
        })?
    }

    // -----------------------------------------------------------------
    // FindNode helper
    // -----------------------------------------------------------------

    /// Returns up to `K` closest peers to `key`, asking the home relay
    /// via `FindNode`. Caches the result for [`FINDNODE_CACHE_TTL`].
    ///
    /// **Why ask the home rather than iterating ourselves**: the
    /// home's routing table converges via anti-entropy; any K-closest
    /// query against it yields a consistent answer. Iterating from
    /// libcore would duplicate the relay-side `dht::lookup::lookup_node`
    /// machinery we already trust. The home is the natural delegate.
    ///
    /// design-doc: `misc/specs/MLS.md` §11.3.
    async fn find_k_closest(
        &self, key: [u8; 32],
    ) -> Result<Vec<NodeDescriptor>, DhtClientError> {
        // Cache hit?
        {
            let cache = self.findnode_cache.lock();
            if let Some(entry) = cache.get(&key)
                && Instant::now().duration_since(entry.cached_at) <= FINDNODE_CACHE_TTL
            {
                return Ok(entry.descriptors.clone());
            }
        }

        // Dial home. **Phase 7 (P0-2)**: pass through `home.pubkey`
        // so the per-dial verifier pins the cert SPKI to the
        // resolver-vended relay pubkey.
        let home = self.home.clone();
        let conn = self.get_or_dial(home.node_id, home.addr, home.pubkey).await?;

        // Our requester id: the ephemeral DhtHello node_id we used on
        // *this connection*. The responder excludes the `requester`
        // field from the FindNode result, so passing our actual id
        // ensures the home doesn't return us back to ourselves —
        // which it would otherwise do because the relay-side
        // `handle_peer_connection` adds the requester to its routing
        // table on DhtHello accept. This matters in sparse-DHT
        // setups (Phase 5b e2e tests) where the libcore-ephemeral
        // could otherwise climb into the K-closest set and confuse
        // the publish fan-out into dialing its own client-side
        // socket. Production dense-DHT statistics make this a
        // non-issue, but the deterministic fix costs nothing.
        let requester = self.ephemeral_for_conn(home.node_id);
        let req = DhtRequest::FindNode(FindNode {
            target:    Bytes(key),
            requester,
        });
        let resp = Self::rpc_one(&conn, req).await?;
        let closer = match resp {
            DhtResponse::FindNode(FindNodeResp { closer }) => closer,
            other => {
                return Err(DhtClientError::Protocol(format!(
                    "expected FindNode response, got {other:?}"
                )));
            },
        };

        // Cache.
        {
            let mut cache = self.findnode_cache.lock();
            cache.insert(
                key,
                FindNodeCacheEntry {
                    descriptors: closer.clone(),
                    cached_at:   Instant::now(),
                },
            );
            // Bound the cache size — a runaway batch send to thousands
            // of distinct recipients shouldn't blow the per-call
            // memory profile. Cap at the same DHT_POOL_MAX (16) since
            // either we have a connection to it (and thus a recent
            // findnode keyed on its IPK) or we're about to.
            if cache.len() > DHT_POOL_MAX {
                // Drop expired entries first; if still over cap, drop
                // the oldest by cached_at.
                let now = Instant::now();
                let expired: Vec<[u8; 32]> = cache
                    .iter()
                    .filter(|(_, e)| now.duration_since(e.cached_at) > FINDNODE_CACHE_TTL)
                    .map(|(k, _)| *k)
                    .collect();
                for k in expired {
                    cache.remove(&k);
                }
                while cache.len() > DHT_POOL_MAX {
                    let oldest = cache
                        .iter()
                        .min_by_key(|(_, e)| e.cached_at)
                        .map(|(k, _)| *k);
                    if let Some(k) = oldest {
                        cache.remove(&k);
                    } else {
                        break;
                    }
                }
            }
        }

        Ok(closer)
    }

    /// Cache size for tests.
    #[cfg(test)]
    pub(crate) fn findnode_cache_size(&self) -> usize {
        self.findnode_cache.lock().len()
    }

    /// Phase 5b: derive the ephemeral DhtHello node_id we used on the
    /// connection to `home_id`. Falls back to `[0u8; 32]` if there's
    /// no live pool entry (rare; typically called right after
    /// `get_or_dial`).
    ///
    /// Used to populate `requester_relay_id` fields in
    /// `KeyPackageFetchReq`, `WelcomeFetchReq`, and `WelcomeAckReq` —
    /// the home cross-checks these against the connection's
    /// authenticated peer id.
    fn ephemeral_for_conn(&self, home_id: NodeId) -> NodeId {
        self.pool
            .lock()
            .entries
            .get(&home_id)
            .map(|e| NodeId::new(e.ephemeral.verifying_key().to_bytes()))
            .unwrap_or_else(|| NodeId::from_bytes([0u8; 32]))
    }

    /// Phase 5b: clear the FindNode cache. Used by the e2e harness's
    /// retry path so a transient stale K-set gets re-fetched on the
    /// next call. Production code never calls this — the 30s TTL is
    /// fine.
    pub fn clear_findnode_cache(&self) {
        self.findnode_cache.lock().clear();
    }
}

// ---------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------

/// Mint a fresh ephemeral Ed25519 keypair. Used per-dial for the
/// DhtHello identity (privacy choice (b)).
pub(crate) fn generate_ephemeral_signer() -> SigningKey {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
}

/// Send our signed [`DhtHello`] on a freshly-opened uni-stream of
/// `conn`. Mirrors `relay/src/dht/lookup.rs::send_dht_hello` — uses
/// the same [`dht_hello_signing_input`] transcript so the relay-side
/// verify is byte-stable.
async fn send_dht_hello(
    conn: &Connection, signer: &SigningKey,
) -> Result<()> {
    let pubkey: [u8; 32] = signer.verifying_key().to_bytes();
    let node_id = NodeId::new(pubkey);
    let timestamp = systime_ms();
    let msg = dht_hello_signing_input(&node_id, &pubkey, timestamp);
    let sig = signer.sign(&msg).to_bytes();

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

/// **Phase 7 (P0-2)**: convert a `Bytes<32>` pubkey from a wire
/// descriptor into a TLS-pinning hint. All-zero values are interpreted
/// as "no pubkey known" (the legacy wire-shape uses `[0u8; 32]` as a
/// placeholder when the routing-table response hasn't carried the
/// real pubkey through). Returning `None` falls back to the un-pinned
/// dial; returning `Some(pk)` enables strict cert-SPKI pinning.
fn pubkey_pin(pk: [u8; 32]) -> Option<[u8; 32]> {
    if pk == [0u8; 32] { None } else { Some(pk) }
}

/// Wall-clock now in ms-since-Unix-epoch.
fn systime_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `BLAKE3("kp:" || ipk)` — same prefix scheme as
/// `relay/src/dht/mls_kp::stash_prefix`; this is what we DhT-route
/// KP records to.
fn kp_stash_key(ipk: &[u8; 32]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(3 + 32);
    buf.extend_from_slice(b"kp:");
    buf.extend_from_slice(ipk);
    *NodeId::new(&buf).as_bytes()
}

/// `BLAKE3("welcome:" || ipk)` — Welcome routing key.
///
/// Note: `relay/src/dht/mls_welcome.rs` keys `cf_dht_welcome` on
/// `MessageKey { recipient: ipk, ts_ms, dispatch_id }` (recipient is
/// the bare IPK, not a digest). Both libcore and the relay agree on
/// the same K-closest set when the FindNode key here matches what the
/// relay computes — which is `ipk` itself (the recipient field on the
/// MessageKey). For libcore we use the same bare IPK as the FindNode
/// key, so the K-set we ship welcomes to is the user's K-homes.
///
/// The Welcome routing path actually doesn't need the `"welcome:"`
/// prefix dance the spec considered (§13.3) — recipient.IPK is the
/// stable routing key. We keep the helper for symmetry and possible
/// future use.
fn welcome_routing_key(recipient_ipk: &[u8; 32]) -> [u8; 32] {
    *recipient_ipk
}

// ---------------------------------------------------------------------
// DhtClient impl
// ---------------------------------------------------------------------

impl DhtClient for Peer1DhtClient {
    async fn publish_keypackages(
        &self, records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        if records.len() > KP_STASH_TARGET {
            return Err(DhtClientError::Protocol(format!(
                "publish_keypackages: batch size {} exceeds KP_STASH_TARGET = {}",
                records.len(),
                KP_STASH_TARGET
            )));
        }
        let our_ipk = self.our_ipk;
        let timestamp = systime_ms();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
        let msg = kp_publish_signing_input(
            MLS_WIRE_VERSION,
            &our_ipk,
            &digest,
            records.len() as u32,
            timestamp,
        );
        let sig = self.our_ipk_signer.sign(&msg).to_bytes();
        let req = KeyPackagePublishReq {
            ipk:       our_ipk.into(),
            records:   records.to_vec(),
            timestamp,
            sig:       Bytes(sig),
        };

        // K-closest of OUR IPK's stash key.
        let homes = self.find_k_closest(kp_stash_key(&our_ipk)).await?;
        // Phase 7 (P0-6): a sparse-DHT publish must NOT silently
        // succeed against a sub-quorum K-set. Mirrors `publish_welcome_to_homes`.
        if homes.len() < K_MIN {
            return Err(DhtClientError::QuorumNotMet {
                succeeded: 0,
                wanted: K_MIN,
            });
        }

        let mut succeeded = 0usize;
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    // Phase 8 (P1 #22): surface dial failures so
                    // operators can debug which homes are unreachable
                    // / cert-mismatched.
                    log::warn!("MLS K-fanout dial to {} failed: {e:?}", home.id);
                    continue;
                }
            };
            let resp = match Self::rpc_one(&conn, DhtRequest::KeyPackagePublish(req.clone()))
                .await
            {
                Ok(r) => r,
                Err(_) => continue,
            };
            match resp {
                DhtResponse::KeyPackagePublish(r)
                    if r.outcome == KeyPackagePublishOutcome::Stored =>
                {
                    succeeded += 1;
                    if succeeded >= K_MIN {
                        return Ok(());
                    }
                },
                _ => continue,
            }
        }

        if succeeded >= K_MIN {
            Ok(())
        } else {
            Err(DhtClientError::QuorumNotMet { succeeded, wanted: K_MIN })
        }
    }

    async fn refill_keypackages(
        &self, records: &[KeyPackageRecord], _filter: KpOutcomeFilter,
    ) -> DhtClientResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        if records.len() > KP_STASH_TARGET {
            return Err(DhtClientError::Protocol(format!(
                "refill_keypackages: batch size {} exceeds KP_STASH_TARGET = {}",
                records.len(),
                KP_STASH_TARGET
            )));
        }
        let our_ipk = self.our_ipk;
        let timestamp = systime_ms();
        let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
        let msg = kp_refill_signing_input(
            MLS_WIRE_VERSION,
            &our_ipk,
            &digest,
            records.len() as u32,
            timestamp,
        );
        let sig = self.our_ipk_signer.sign(&msg).to_bytes();
        let req = KeyPackageRefillReq {
            ipk:       our_ipk.into(),
            records:   records.to_vec(),
            timestamp,
            sig:       Bytes(sig),
        };

        let homes = self.find_k_closest(kp_stash_key(&our_ipk)).await?;
        // Phase 7 (P0-6): same sub-quorum hard-fail as `publish_keypackages`.
        if homes.len() < K_MIN {
            return Err(DhtClientError::QuorumNotMet {
                succeeded: 0,
                wanted: K_MIN,
            });
        }
        let mut succeeded = 0usize;
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    // Phase 8 (P1 #22): surface dial failures so
                    // operators can debug which homes are unreachable
                    // / cert-mismatched.
                    log::warn!("MLS K-fanout dial to {} failed: {e:?}", home.id);
                    continue;
                }
            };
            let resp = match Self::rpc_one(&conn, DhtRequest::KeyPackageRefill(req.clone()))
                .await
            {
                Ok(r) => r,
                Err(_) => continue,
            };
            match resp {
                DhtResponse::KeyPackageRefill(r)
                    if r.outcome == KeyPackageRefillOutcome::Appended =>
                {
                    succeeded += 1;
                    if succeeded >= K_MIN {
                        return Ok(());
                    }
                },
                _ => continue,
            }
        }
        if succeeded >= K_MIN {
            Ok(())
        } else {
            Err(DhtClientError::QuorumNotMet { succeeded, wanted: K_MIN })
        }
    }

    async fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> DhtClientResult<FetchedKeyPackage> {
        let homes = self.find_k_closest(kp_stash_key(target_ipk)).await?;
        if homes.is_empty() {
            return Err(DhtClientError::NoStash);
        }

        let mut last_transport: Option<DhtClientError> = None;
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    last_transport = Some(e);
                    continue;
                },
            };
            // The `requester_relay_id` field is cross-checked at the
            // home against the connection's authenticated peer id (the
            // DhtHello node_id we sent at dial time). Pass our
            // ephemeral identity, NOT the home's own id, otherwise the
            // home rejects with `RateLimited` (Phase 5b bugfix).
            let requester = self.ephemeral_for_conn(home.id);
            let req = KeyPackageFetchReq {
                target_ipk:         (*target_ipk).into(),
                requester_relay_id: requester,
                timestamp:          systime_ms(),
            };
            let resp = match Self::rpc_one(&conn, DhtRequest::KeyPackageFetch(req)).await {
                Ok(r) => r,
                Err(e) => {
                    last_transport = Some(e);
                    continue;
                },
            };
            let outcome = match resp {
                DhtResponse::KeyPackageFetch(r) => r.outcome,
                other => {
                    return Err(DhtClientError::Protocol(format!(
                        "expected KeyPackageFetch response, got {other:?}"
                    )));
                },
            };
            match outcome {
                KeyPackageFetchOutcome::Found(found) => {
                    return Ok(FetchedKeyPackage {
                        record:      found.record,
                        remaining:   found.remaining,
                        static_hash: found.static_hash.0,
                    });
                },
                KeyPackageFetchOutcome::NoStash => continue,
                KeyPackageFetchOutcome::NotOwner => continue,
                KeyPackageFetchOutcome::RateLimited => continue,
            }
        }
        if let Some(e) = last_transport {
            Err(e)
        } else {
            Err(DhtClientError::NoStash)
        }
    }

    async fn publish_welcome_to_homes(
        &self, envelope: &WelcomeEnvelopeP,
    ) -> DhtClientResult<PublishOutcome> {
        let recipient_ipk: [u8; 32] = envelope.recipient_ipk.0;
        let homes = self.find_k_closest(welcome_routing_key(&recipient_ipk)).await?;
        // Phase 7 (P0-3, P0-6): a sparse home set must surface as a
        // hard quorum failure — the previous `Ok(PublishOutcome::Failed)`
        // / `homes.len().min(3)` pair silently downgraded to "K_MIN of
        // whatever we found", which let a single-home DHT pretend the
        // Welcome was published while the recipient never received it.
        if homes.len() < K_MIN {
            return Err(DhtClientError::QuorumNotMet {
                succeeded: 0,
                wanted: K_MIN,
            });
        }

        let timestamp = systime_ms();
        let req = WelcomePublishReq {
            envelope: envelope.clone(),
            timestamp,
        };

        let mut succeeded = 0usize;
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    // Phase 8 (P1 #22): surface dial failures so
                    // operators can debug which homes are unreachable
                    // / cert-mismatched.
                    log::warn!("MLS K-fanout dial to {} failed: {e:?}", home.id);
                    continue;
                }
            };
            let resp = match Self::rpc_one(
                &conn,
                DhtRequest::WelcomePublish(req.clone()),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => continue,
            };
            match resp {
                DhtResponse::WelcomePublish(r)
                    if r.outcome == WelcomePublishOutcome::Stored =>
                {
                    succeeded += 1;
                    if succeeded >= K_MIN {
                        return Ok(PublishOutcome::Stored);
                    }
                },
                _ => continue,
            }
        }
        if succeeded >= K_MIN {
            Ok(PublishOutcome::Stored)
        } else {
            Err(DhtClientError::QuorumNotMet {
                succeeded,
                wanted: K_MIN,
            })
        }
    }

    async fn fetch_welcomes(&self) -> DhtClientResult<Vec<WelcomeEntry>> {
        let our_ipk = self.our_ipk;
        let homes = self.find_k_closest(welcome_routing_key(&our_ipk)).await?;
        if homes.is_empty() {
            return Ok(Vec::new());
        }

        let mut all: Vec<WelcomeEntry> = Vec::new();
        let mut seen_ids: std::collections::HashSet<[u8; 8]> =
            std::collections::HashSet::new();
        let mut any_success = false;
        let mut last_err: Option<DhtClientError> = None;
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                },
            };
            // Phase 5b: same `requester_relay_id` correction as
            // `fetch_keypackage_for` — pass the libcore-ephemeral
            // node_id so the home's cross-check against
            // `authenticated_peer_id` succeeds.
            let requester = self.ephemeral_for_conn(home.id);
            let timestamp = systime_ms();
            let transcript = welcome_fetch_signing_input(
                MLS_WIRE_VERSION,
                &our_ipk,
                &requester,
                timestamp,
            );
            let user_sig = self.our_ipk_signer.sign(&transcript).to_bytes();
            let req = WelcomeFetchReq {
                user_ipk:           our_ipk.into(),
                requester_relay_id: requester,
                timestamp,
                user_sig:           Bytes(user_sig),
            };
            let resp = match Self::rpc_one(&conn, DhtRequest::WelcomeFetch(req)).await {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                },
            };
            match resp {
                DhtResponse::WelcomeFetch(r) => match r.outcome {
                    WelcomeFetchOutcome::Found(found) => {
                        any_success = true;
                        for entry in found.welcomes {
                            if seen_ids.insert(entry.welcome_id.0) {
                                all.push(entry);
                            }
                        }
                    },
                    WelcomeFetchOutcome::BadSig
                    | WelcomeFetchOutcome::NotOwner
                    | WelcomeFetchOutcome::RateLimited => {
                        // Try the next home — degraded responses don't
                        // necessarily mean nothing's there. Surface the
                        // failure if every home fails.
                        last_err = Some(DhtClientError::Protocol(format!(
                            "WelcomeFetch outcome: {:?}", r.outcome
                        )));
                        continue;
                    },
                },
                other => {
                    last_err = Some(DhtClientError::Protocol(format!(
                        "expected WelcomeFetch response, got {other:?}"
                    )));
                    continue;
                },
            }
        }
        if any_success {
            Ok(all)
        } else if let Some(e) = last_err {
            Err(e)
        } else {
            Ok(Vec::new())
        }
    }

    async fn ack_welcomes(
        &self, welcome_ids: &[[u8; 8]],
    ) -> DhtClientResult<()> {
        if welcome_ids.is_empty() {
            return Ok(());
        }
        let our_ipk = self.our_ipk;
        let homes = self.find_k_closest(welcome_routing_key(&our_ipk)).await?;
        if homes.is_empty() {
            return Ok(()); // best-effort
        }

        // Best-effort fan-out — a missing ack just leaves a TTL'd entry
        // at the home; the recipient's local processed-set prevents
        // double-handling. We attempt all K homes and ignore failures.
        for home in homes.iter().take(3) {
            let conn = match self.get_or_dial(
                home.id,
                home.addr,
                pubkey_pin(home.pubkey.0),
            ).await {
                Ok(c) => c,
                Err(e) => {
                    // Phase 8 (P1 #22): surface dial failures so
                    // operators can debug which homes are unreachable
                    // / cert-mismatched.
                    log::warn!("MLS K-fanout dial to {} failed: {e:?}", home.id);
                    continue;
                }
            };
            // Phase 5b: requester_relay_id is the libcore-ephemeral.
            let requester = self.ephemeral_for_conn(home.id);
            let timestamp = systime_ms();
            let transcript = welcome_ack_signing_input(
                MLS_WIRE_VERSION,
                &our_ipk,
                &requester,
                welcome_ids,
                timestamp,
            );
            let user_sig = self.our_ipk_signer.sign(&transcript).to_bytes();
            let req = WelcomeAckReq {
                user_ipk:           our_ipk.into(),
                requester_relay_id: requester,
                welcome_ids:        welcome_ids
                    .iter()
                    .map(|id| Bytes(*id))
                    .collect(),
                timestamp,
                user_sig:           Bytes(user_sig),
            };
            let _ = Self::rpc_one(&conn, DhtRequest::WelcomeAck(req)).await;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Resolve an IpAddr from a host string. Used to translate the libcore
// `Relay::host` string into a `SocketAddr` for dialing.
// ---------------------------------------------------------------------

/// Build a [`HomeDescriptor`] from libcore's [`crate::data::relay::Relay`]
/// shape.
///
/// **Phase 8 (P0-2 residual)**: pubkey is now persisted on the libcore
/// `Relay` row by `Relay::refresh` (the resolver vends it in
/// `RelayDescriptor.pubkey`). When present, the home dial's TLS verifier
/// pins the cert SPKI to the resolver-vended value, defeating a
/// network MitM. When absent (rows pre-dating the schema migration,
/// or a resolver that doesn't carry the field), we fall back to the
/// un-pinned verifier and emit a `log::warn` so operators notice.
pub fn home_from_relay(
    relay: &crate::data::relay::Relay,
) -> Result<HomeDescriptor> {
    if relay.pubkey.is_none() {
        log::warn!(
            "MLS: home relay {} has no persisted pubkey; peer/1 dial will skip SPKI pinning",
            relay.id
        );
    }
    home_from_relay_with_pubkey(relay, relay.pubkey)
}

/// **Phase 7 (P0-2)**: pubkey-aware variant of [`home_from_relay`].
/// Pass `Some(pubkey)` to enable per-dial cert-SPKI pinning on the
/// home connection.
pub fn home_from_relay_with_pubkey(
    relay: &crate::data::relay::Relay, pubkey: Option<[u8; 32]>,
) -> Result<HomeDescriptor> {
    let node_id = NodeId::from_str(&relay.id)
        .map_err(|e| anyhow!("relay.id not parseable as NodeId: {e}"))?;
    let ip = IpAddr::from_str(&relay.host)
        .map_err(|e| anyhow!("relay.host not parseable as IP: {e}"))?;
    let addr = SocketAddr::new(ip, relay.port);
    Ok(HomeDescriptor { node_id, addr, pubkey })
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Wire-level integration tests against a small fake `peer/1`
    //! acceptor. The acceptor implements just enough of the
    //! relay-side protocol to exercise the dialer end-to-end:
    //! TLS handshake, signed `DhtHello` accept (id-binding +
    //! signature verify), then per-bi-stream `DhtRequest` →
    //! `DhtResponse` round-trips.
    //!
    //! This avoids dragging the full `relay` crate into libcore's
    //! dev-deps (which would pull RocksDB + governor + etc.). The
    //! fake server's wire behaviour comes from the same
    //! [`common::proto`] codecs the production relay uses, so the
    //! test is byte-stable against the real server.
    //!
    //! Under the test harness, `quinn::Endpoint` listens on
    //! `127.0.0.1:0` (kernel-chosen port). The TLS server side uses
    //! the existing libcore `build_peer_server_cfg` adapted with a
    //! fresh ephemeral identity.

    use std::collections::VecDeque;
    use std::net::SocketAddr;

    use common::proto::dht_p2p::Ping;
    use common::proto::dht_p2p::Pong;
    use common::proto::mls_wire::KeyPackageFetchFound;
    use common::proto::mls_wire::KeyPackageFetchOutcome;
    use common::proto::mls_wire::KeyPackageFetchResp;
    use common::proto::mls_wire::KeyPackagePublishOutcome;
    use common::proto::mls_wire::KeyPackagePublishResp;
    use common::proto::mls_wire::WelcomeAckResp;
    use common::proto::mls_wire::WelcomeEntry;
    use common::proto::mls_wire::WelcomeFetchFound;
    use common::proto::mls_wire::WelcomeFetchOutcome;
    use common::proto::mls_wire::WelcomeFetchResp;
    use common::proto::mls_wire::WelcomePublishOutcome;
    use common::proto::mls_wire::WelcomePublishResp;
    use common::types::bytes::ByteVec;

    use super::*;

    /// Per-test record of a request the fake server received.
    #[derive(Debug, Clone)]
    pub(crate) struct ReceivedRpc {
        pub req:        DhtRequest,
        pub hello_node: NodeId,
    }

    /// Programmable response queue for the fake server — one response
    /// is popped per inbound request. Tests prime this with the
    /// outcomes they expect.
    #[derive(Default)]
    struct ResponderState {
        queue: parking_lot::Mutex<VecDeque<DhtResponse>>,
        log:   parking_lot::Mutex<Vec<ReceivedRpc>>,
        /// Counts dial-time DhtHello receptions (one per QUIC dial).
        dials: parking_lot::Mutex<usize>,
        /// Per-dial DhtHello node_ids (so tests can verify ephemeral
        /// keys produce distinct identities).
        hello_node_ids: parking_lot::Mutex<Vec<NodeId>>,
    }

    impl ResponderState {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn enqueue(&self, resp: DhtResponse) {
            self.queue.lock().push_back(resp);
        }

        fn dials_count(&self) -> usize {
            *self.dials.lock()
        }

        fn record_log(&self) -> Vec<ReceivedRpc> {
            self.log.lock().clone()
        }

        fn hello_node_ids(&self) -> Vec<NodeId> {
            self.hello_node_ids.lock().clone()
        }
    }

    /// Idempotent install of the crypto provider; the rustls
    /// `aws-lc-rs` provider must be set process-globally before any
    /// `quinn::Endpoint::server` / `quinn::Endpoint::client` call. The
    /// production code path runs this from `lib.rs::JNI_OnLoad`; tests
    /// re-do it on every fresh test process.
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );
    }

    /// Spawn a fake `peer/1` acceptor. Returns its bound `(NodeId,
    /// SocketAddr)` plus the responder state for assertions.
    async fn spawn_fake_peer1(
        responder: Arc<ResponderState>,
    ) -> (NodeId, SocketAddr) {
        ensure_crypto_provider();
        // Server identity: fresh ephemeral (the fake doesn't model a
        // production relay). The server cert we use is the
        // self-signed Ed25519 cert built by libcore's
        // `peer_config::generate_identity_cert`-equivalent path. We
        // can't directly call that helper (it relies on JNI-backed
        // `IdentitySigner`), so we hand-roll a minimal server config
        // here — the libcore `PeerServerCertVerifier` accepts any
        // valid Ed25519 cert.
        let signing = generate_ephemeral_signer();
        let server_cfg = build_test_peer_server_cfg(&signing);
        let endpoint =
            quinn::Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let node_id = NodeId::new(signing.verifying_key().to_bytes());

        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let responder = responder.clone();
                tokio::spawn(async move {
                    let conn = match incoming.await {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    *responder.dials.lock() += 1;
                    // Read & verify DhtHello.
                    let hello_node = match read_and_verify_hello(&conn).await {
                        Ok(node_id) => {
                            responder.hello_node_ids.lock().push(node_id);
                            node_id
                        },
                        Err(_) => return,
                    };
                    // Bi-stream RPC loop.
                    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let req_pkt = match DhtPacket::unpack(&mut recv).await {
                            Ok(p) => p,
                            Err(_) => break,
                        };
                        let req = match req_pkt {
                            DhtPacket::Request(r) => r,
                            DhtPacket::Response(_) => break,
                        };
                        responder.log.lock().push(ReceivedRpc {
                            req:        req.clone(),
                            hello_node,
                        });
                        let resp = match responder.queue.lock().pop_front() {
                            Some(r) => r,
                            None => DhtResponse::Pong(Pong {
                                nonce:     Bytes([0; 16]),
                                timestamp: 0,
                            }),
                        };
                        let bytes = match DhtPacket::Response(resp).pack() {
                            Ok(b) => b,
                            Err(_) => break,
                        };
                        if send.write_all(&bytes).await.is_err() {
                            break;
                        }
                        let _ = send.finish();
                    }
                });
            }
        });
        (node_id, addr)
    }

    /// Read the dialer's first uni-stream as a `DhtHello`, verify
    /// `BLAKE3(pubkey) == node_id` and the signature, return the
    /// authenticated NodeId.
    async fn read_and_verify_hello(
        conn: &quinn::Connection,
    ) -> Result<NodeId> {
        let mut recv = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            conn.accept_uni(),
        )
        .await
        .map_err(|_| anyhow!("hello accept_uni timeout"))?
        .map_err(|e| anyhow!("hello accept_uni failed: {e}"))?;
        let hello: DhtHello = DhtHello::unpack(&mut recv)
            .await
            .map_err(|e| anyhow!("hello unpack: {e}"))?;
        let now = systime_ms();
        hello
            .verify(now)
            .map_err(|e| anyhow!("hello verify: {e:?}"))?;
        Ok(hello.node_id)
    }

    /// Build a minimal `quinn::ServerConfig` for the fake peer/1
    /// acceptor. Self-signed Ed25519 cert, accepts any client (no
    /// client auth), `peer/1` ALPN.
    fn build_test_peer_server_cfg(
        signing: &SigningKey,
    ) -> quinn::ServerConfig {
        let pubkey = signing.verifying_key().to_bytes();
        let tbs = build_tbs_cert(&pubkey);
        let sig = signing.sign(&tbs);
        let cert_der = build_cert_der(&tbs, &sig.to_bytes());
        let cert = rustls::pki_types::CertificateDer::from(cert_der);

        // The cert comes paired with the private key for handshake
        // signing. rustls' `SignatureScheme::ED25519` requires an
        // Ed25519 signing key wrapper. We use the same trick as
        // libcore's `IdentitySigningKey`: a small struct implementing
        // `rustls::sign::SigningKey`.
        let signing_key: Arc<dyn rustls::sign::SigningKey> =
            Arc::new(TestEd25519Signer { signing: signing.clone() });
        let certified = rustls::sign::CertifiedKey::new(vec![cert], signing_key);

        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                certified,
            )));
        tls.alpn_protocols = vec![common::quic::protorole::ProtoRole::Peer.alpn().into()];
        let q = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(q));
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        cfg.transport_config(Arc::new(transport));
        cfg
    }

    /// Self-signed cert builder — same shape as libcore's
    /// `peer_config::build_tbs_certificate` / `build_certificate_der`,
    /// duplicated here because those are private to that module.
    fn build_tbs_cert(public_key: &[u8; 32]) -> Vec<u8> {
        let ed25519_oid: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
        let spki = [
            &[0x30, 0x2a][..],
            &[0x30, 0x05][..],
            ed25519_oid,
            &[0x03, 0x21, 0x00][..],
            public_key,
        ]
        .concat();
        let serial = &public_key[0..8];
        let validity: &[u8] = &[
            0x30, 0x1e, 0x17, 0x0d, b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0',
            b'0', b'0', b'0', b'Z', 0x17, 0x0d, b'5', b'0', b'0', b'1', b'0', b'1', b'0',
            b'0', b'0', b'0', b'0', b'0', b'Z',
        ];
        let empty_name: &[u8] = &[0x30, 0x00];
        let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];
        let serial_der = [&[0x02, serial.len() as u8][..], serial].concat();
        let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
        let tbs_content = [
            version,
            &serial_der,
            sig_alg,
            empty_name,
            validity,
            empty_name,
            &spki,
        ]
        .concat();
        encode_seq(&tbs_content)
    }

    fn build_cert_der(tbs: &[u8], signature: &[u8; 64]) -> Vec<u8> {
        let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
        let sig_bitstring = [&[0x03, 0x41, 0x00][..], signature].concat();
        let cert_content = [tbs, sig_alg, &sig_bitstring].concat();
        encode_seq(&cert_content)
    }

    fn encode_seq(data: &[u8]) -> Vec<u8> {
        let len = data.len();
        if len < 128 {
            [&[0x30, len as u8][..], data].concat()
        } else if len < 256 {
            [&[0x30, 0x81, len as u8][..], data].concat()
        } else {
            let len_bytes = (len as u16).to_be_bytes();
            [&[0x30, 0x82][..], &len_bytes, data].concat()
        }
    }

    /// rustls `SigningKey` impl that signs with a stored Ed25519
    /// secret. The fake server uses this; tests don't go through the
    /// JNI key-manager.
    #[derive(Debug)]
    struct TestEd25519Signer {
        signing: SigningKey,
    }

    impl rustls::sign::SigningKey for TestEd25519Signer {
        fn choose_scheme(
            &self, offered: &[rustls::SignatureScheme],
        ) -> Option<Box<dyn rustls::sign::Signer>> {
            if offered.contains(&rustls::SignatureScheme::ED25519) {
                Some(Box::new(TestEd25519Inner {
                    signing: self.signing.clone(),
                }))
            } else {
                None
            }
        }

        fn public_key(&self) -> Option<rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
            let alg_id = rustls::pki_types::AlgorithmIdentifier::from_slice(&[
                0x06, 0x03, 0x2b, 0x65, 0x70,
            ]);
            let pubkey: [u8; 32] = self.signing.verifying_key().to_bytes();
            Some(rustls::sign::public_key_to_spki(&alg_id, pubkey))
        }

        fn algorithm(&self) -> rustls::SignatureAlgorithm {
            rustls::SignatureAlgorithm::ED25519
        }
    }

    #[derive(Debug)]
    struct TestEd25519Inner {
        signing: SigningKey,
    }

    impl rustls::sign::Signer for TestEd25519Inner {
        fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
            Ok(self.signing.sign(message).to_bytes().to_vec())
        }

        fn scheme(&self) -> rustls::SignatureScheme {
            rustls::SignatureScheme::ED25519
        }
    }

    /// Build a libcore-side `quinn::Endpoint` with peer/1 ALPN client
    /// config that accepts any Ed25519 cert (the production
    /// `PeerServerCertVerifier`). Since libcore's
    /// `build_peer_client_cfg` reaches into the JNI-backed
    /// `IdentitySigner::tls_subkey`, we can't use it directly; we
    /// build an equivalent client config from a fresh ephemeral
    /// signer.
    fn build_test_client_endpoint() -> (Endpoint, Arc<ClientConfig>) {
        // Install crypto provider (idempotent — common does this on
        // production startup).
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );

        // Client side: any Ed25519 cert from a fresh ephemeral; the
        // server config is `with_no_client_auth` so the cert isn't
        // sent over the wire anyway.
        let client_signing = generate_ephemeral_signer();
        let pubkey = client_signing.verifying_key().to_bytes();
        let tbs = build_tbs_cert(&pubkey);
        let sig = client_signing.sign(&tbs);
        let cert_der = build_cert_der(&tbs, &sig.to_bytes());
        let cert = rustls::pki_types::CertificateDer::from(cert_der);
        let signing_key: Arc<dyn rustls::sign::SigningKey> =
            Arc::new(TestEd25519Signer { signing: client_signing });
        let certified = rustls::sign::CertifiedKey::new(vec![cert], signing_key);

        let mut tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TestServerCertVerifier))
            .with_client_cert_resolver(Arc::new(
                rustls::sign::SingleCertAndKey::from(certified),
            ));
        tls.alpn_protocols =
            vec![common::quic::protorole::ProtoRole::Peer.alpn().into()];
        let q = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut client_cfg = ClientConfig::new(Arc::new(q));
        let mut transport = quinn::TransportConfig::default();
        transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
        client_cfg.transport_config(Arc::new(transport));

        let endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        (endpoint, Arc::new(client_cfg))
    }

    /// Permissive server-cert verifier — accepts any Ed25519 cert.
    /// Mirrors libcore's `PeerServerCertVerifier`.
    #[derive(Debug)]
    struct TestServerCertVerifier;

    impl rustls::client::danger::ServerCertVerifier for TestServerCertVerifier {
        fn verify_server_cert(
            &self, _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>, _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self, _msg: &[u8], _crt: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Err(rustls::Error::General("TLS 1.2 not supported".into()))
        }

        fn verify_tls13_signature(
            &self, msg: &[u8], cert: &rustls::pki_types::CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            // Extract Ed25519 SPKI and verify dss.signature() under it.
            let pubkey = ed25519_pubkey_from_cert(cert.as_ref())
                .ok_or_else(|| rustls::Error::General("not an Ed25519 cert".into()))?;
            if dss.scheme != rustls::SignatureScheme::ED25519 {
                return Err(rustls::Error::General("scheme mismatch".into()));
            }
            let vk = ed25519_dalek::VerifyingKey::from_bytes(&pubkey)
                .map_err(|e| rustls::Error::General(format!("bad SPKI: {e}")))?;
            let sig_bytes: &[u8] = dss.signature();
            let sig_arr: [u8; 64] = sig_bytes
                .try_into()
                .map_err(|_| rustls::Error::General("sig len".into()))?;
            let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
            use ed25519_dalek::Verifier;
            vk.verify(msg, &sig).map_err(|e| {
                rustls::Error::General(format!("ed25519 sig: {e}"))
            })?;
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![rustls::SignatureScheme::ED25519]
        }
    }

    /// Extract Ed25519 SPKI from a DER cert. Tiny re-impl of
    /// libcore's `peer_config::ed25519_pubkey_from_cert_der`.
    fn ed25519_pubkey_from_cert(cert_der: &[u8]) -> Option<[u8; 32]> {
        use x509_parser::prelude::FromDer;
        use x509_parser::prelude::X509Certificate;
        let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
        let spki = cert.public_key();
        let oid_ed25519: x509_parser::der_parser::Oid<'_> =
            x509_parser::oid_registry::asn1_rs::oid!(1.3.101 .112);
        if spki.algorithm.algorithm != oid_ed25519 {
            return None;
        }
        let raw: &[u8] = &spki.subject_public_key.data;
        if raw.len() != 32 {
            return None;
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(raw);
        Some(out)
    }

    /// Build a `Peer1DhtClient` with a fresh user signing key, the
    /// fake peer1 endpoint, and a stub home descriptor.
    fn build_test_client_against(
        addr: SocketAddr, home_node_id: NodeId,
    ) -> (Arc<Peer1DhtClient>, [u8; 32], SigningKey) {
        let (endpoint, peer_cfg) = build_test_client_endpoint();
        let our_signer = SigningKey::from_bytes(&[0x77; 32]);
        let our_ipk = our_signer.verifying_key().to_bytes();
        let home = HomeDescriptor {
            node_id: home_node_id,
            addr,
            pubkey:  None,
        };
        let client =
            Peer1DhtClient::new_arc(endpoint, peer_cfg, home, our_ipk, our_signer.clone());
        (client, our_ipk, our_signer)
    }

    // ----------------------------------------------------------------
    // Tests
    // ----------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ephemeral_keypair_per_dial_yields_distinct_node_ids() {
        let responder = ResponderState::new();
        // Two echo Pong responses for the two RPCs we'll fire.
        responder.enqueue(DhtResponse::Pong(Pong {
            nonce:     Bytes([1; 16]),
            timestamp: 0,
        }));
        let (server_id, addr) = spawn_fake_peer1(responder.clone()).await;

        let (client, _our_ipk, _signer) = build_test_client_against(addr, server_id);

        // Two separate dials -> two ephemeral hellos. We get two dials
        // by going through `get_or_dial` for two distinct NodeIds —
        // but our fake server only has one NodeId. Instead, force two
        // dials by clearing the pool between calls.
        let conn1 = client.get_or_dial(server_id, addr, None).await.unwrap();
        let _r1 = Peer1DhtClient::rpc_one(
            &conn1,
            DhtRequest::Ping(Ping {
                nonce:     Bytes([0; 16]),
                timestamp: 0,
            }),
        )
        .await
        .unwrap();

        // Drop the cached entry to force re-dial.
        {
            let mut pool = client.pool.lock();
            pool.entries.clear();
            pool.order.clear();
        }

        responder.enqueue(DhtResponse::Pong(Pong {
            nonce:     Bytes([2; 16]),
            timestamp: 0,
        }));
        let conn2 = client.get_or_dial(server_id, addr, None).await.unwrap();
        let _r2 = Peer1DhtClient::rpc_one(
            &conn2,
            DhtRequest::Ping(Ping {
                nonce:     Bytes([0; 16]),
                timestamp: 0,
            }),
        )
        .await
        .unwrap();

        let hellos = responder.hello_node_ids();
        assert_eq!(hellos.len(), 2);
        assert_ne!(hellos[0], hellos[1], "ephemeral keys must differ across dials");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_pool_reuses_cached_dial_for_repeat_calls() {
        let responder = ResponderState::new();
        // Reply to two RPCs with Pongs.
        responder.enqueue(DhtResponse::Pong(Pong {
            nonce:     Bytes([0; 16]),
            timestamp: 0,
        }));
        responder.enqueue(DhtResponse::Pong(Pong {
            nonce:     Bytes([0; 16]),
            timestamp: 0,
        }));
        let (server_id, addr) = spawn_fake_peer1(responder.clone()).await;

        let (client, _our_ipk, _signer) = build_test_client_against(addr, server_id);

        let conn1 = client.get_or_dial(server_id, addr, None).await.unwrap();
        let _r1 = Peer1DhtClient::rpc_one(
            &conn1,
            DhtRequest::Ping(Ping {
                nonce:     Bytes([0; 16]),
                timestamp: 0,
            }),
        )
        .await
        .unwrap();
        let conn2 = client.get_or_dial(server_id, addr, None).await.unwrap();
        let _r2 = Peer1DhtClient::rpc_one(
            &conn2,
            DhtRequest::Ping(Ping {
                nonce:     Bytes([0; 16]),
                timestamp: 0,
            }),
        )
        .await
        .unwrap();

        // Only one dial happened.
        assert_eq!(responder.dials_count(), 1);
        assert_eq!(client.pool_size(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn connection_pool_lru_evicts_oldest_at_capacity() {
        // Spin up 17 fake servers; dial them one by one. After the
        // 17th, the LRU should have evicted the first.
        let mut servers = Vec::new();
        for _ in 0..17 {
            let r = ResponderState::new();
            // Each server replies with a Pong to whatever we send.
            for _ in 0..2 {
                r.enqueue(DhtResponse::Pong(Pong {
                    nonce:     Bytes([0; 16]),
                    timestamp: 0,
                }));
            }
            let (sid, addr) = spawn_fake_peer1(r.clone()).await;
            servers.push((sid, addr, r));
        }

        // Build a client; treat the first server as "home" for the
        // descriptor (we won't actually FindNode through it; we dial
        // directly).
        let home_id = servers[0].0;
        let home_addr = servers[0].1;
        let (client, _our_ipk, _signer) = build_test_client_against(home_addr, home_id);

        // Dial all 17 in order.
        let mut conns = Vec::new();
        for (sid, addr, _) in &servers {
            let c = client.get_or_dial(*sid, *addr, None).await.unwrap();
            conns.push(c);
        }

        // Pool capped at DHT_POOL_MAX (16); the first dialed peer
        // should be evicted.
        assert_eq!(client.pool_size(), DHT_POOL_MAX);
        let order = client.test_pool_order();
        assert_eq!(order.len(), DHT_POOL_MAX);
        assert!(
            !order.iter().any(|id| *id == servers[0].0),
            "first-dialed peer must be evicted"
        );
        // Last-dialed must be at the back of the LRU (most recent).
        assert_eq!(order.last().copied(), Some(servers[16].0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_pool_idle_ttl_drops_entries() {
        let responder = ResponderState::new();
        let (server_id, addr) = spawn_fake_peer1(responder.clone()).await;

        let (client, _our_ipk, _signer) = build_test_client_against(addr, server_id);

        let _conn = client.get_or_dial(server_id, addr, None).await.unwrap();
        assert_eq!(client.pool_size(), 1);

        // Age every entry past the TTL, then sweep.
        client.test_age_pool_entries(DHT_CONN_IDLE_TTL + Duration::from_secs(1));
        client.test_evict_expired();
        assert_eq!(client.pool_size(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn key_package_publish_round_trip_against_fake_server() {
        use common::types::bytes::ByteVec;

        // We need two distinct relay NodeIds for the K-quorum test
        // (the pool dedupes by NodeId so two descriptors with the
        // same id would only dial once). Spin up two fakes.
        let r1 = ResponderState::new();
        let r2 = ResponderState::new();
        // Each fake receives one publish; both reply Stored.
        r1.enqueue(DhtResponse::KeyPackagePublish(KeyPackagePublishResp {
            outcome: KeyPackagePublishOutcome::Stored,
        }));
        r2.enqueue(DhtResponse::KeyPackagePublish(KeyPackagePublishResp {
            outcome: KeyPackagePublishOutcome::Stored,
        }));
        let (sid1, addr1) = spawn_fake_peer1(r1.clone()).await;
        let (sid2, addr2) = spawn_fake_peer1(r2.clone()).await;

        // Treat sid1 as the "home" descriptor for the libcore client
        // (we'll prime the FindNode cache so the home is never
        // actually asked).
        let (client, our_ipk, our_signer) = build_test_client_against(addr1, sid1);

        use common::proto::mls_wire::kp_record_signing_input;
        let kp_ref = vec![0xAA; 32];
        let kp_bytes = vec![0xBB; 16];
        let expires_at_ms = systime_ms() + 24 * 3_600_000;
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &our_ipk,
            &kp_ref,
            &kp_bytes,
            expires_at_ms,
        );
        let sig = our_signer.sign(&msg).to_bytes();
        let rec = KeyPackageRecord {
            ipk:           our_ipk.into(),
            kp_ref:        ByteVec(kp_ref),
            kp_bytes:      ByteVec(kp_bytes),
            expires_at_ms,
            owner_sig:     Bytes(sig),
        };

        // Prime cache: two distinct descriptors so we dial both and
        // reach K_MIN=2.
        client.findnode_cache.lock().insert(
            kp_stash_key(&our_ipk),
            FindNodeCacheEntry {
                descriptors: vec![
                    NodeDescriptor {
                        id:     sid1,
                        addr:   addr1,
                        pubkey: Bytes([0u8; 32]),
                    },
                    NodeDescriptor {
                        id:     sid2,
                        addr:   addr2,
                        pubkey: Bytes([0u8; 32]),
                    },
                ],
                cached_at:   Instant::now(),
            },
        );

        let outcome = client
            .publish_keypackages(&[rec], KpOutcomeFilter::Default)
            .await;
        assert!(outcome.is_ok(), "publish quorum reached: {outcome:?}");

        // Each fake should have received exactly one publish.
        assert_eq!(r1.record_log().len(), 1);
        assert_eq!(r2.record_log().len(), 1);
        for entry in r1.record_log().into_iter().chain(r2.record_log()) {
            match entry.req {
                DhtRequest::KeyPackagePublish(_) => {},
                other => panic!("expected publish, got {other:?}"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_package_fetch_round_trip_against_fake_server() {
        let responder = ResponderState::new();
        let (server_id, addr) = spawn_fake_peer1(responder.clone()).await;

        let (client, our_ipk, _our_signer) =
            build_test_client_against(addr, server_id);

        let target_ipk: [u8; 32] = [0xCD; 32];

        // Prime FindNode cache.
        let descs = vec![NodeDescriptor {
            id:     server_id,
            addr,
            pubkey: Bytes([0u8; 32]),
        }];
        client.findnode_cache.lock().insert(
            kp_stash_key(&target_ipk),
            FindNodeCacheEntry {
                descriptors: descs,
                cached_at:   Instant::now(),
            },
        );

        // Build a fetch response containing a sample record.
        use common::proto::mls_wire::kp_record_signing_input;
        let target_signer = SigningKey::from_bytes(&[0xCC; 32]);
        let target_pub = target_signer.verifying_key().to_bytes();
        let kp_ref = vec![0xEF; 32];
        let kp_bytes = vec![0xAB; 16];
        let expires_at_ms = systime_ms() + 24 * 3_600_000;
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &target_pub,
            &kp_ref,
            &kp_bytes,
            expires_at_ms,
        );
        let sig = target_signer.sign(&msg).to_bytes();
        let rec = KeyPackageRecord {
            ipk:           target_pub.into(),
            kp_ref:        ByteVec(kp_ref.clone()),
            kp_bytes:      ByteVec(kp_bytes),
            expires_at_ms,
            owner_sig:     Bytes(sig),
        };
        responder.enqueue(DhtResponse::KeyPackageFetch(KeyPackageFetchResp {
            outcome: KeyPackageFetchOutcome::Found(KeyPackageFetchFound {
                record:      rec.clone(),
                remaining:   42,
                static_hash: Bytes([0u8; 32]),
            }),
        }));

        let fetched = client.fetch_keypackage_for(&target_ipk).await.unwrap();
        assert_eq!(fetched.record, rec);
        assert_eq!(fetched.remaining, 42);
        let _ = our_ipk;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn welcome_publish_fetch_ack_round_trip() {
        // Two fakes for the publish-quorum (recipient's K-set), one
        // for the fetch/ack of our-own welcomes.
        let r_pub_a = ResponderState::new();
        let r_pub_b = ResponderState::new();
        r_pub_a.enqueue(DhtResponse::WelcomePublish(WelcomePublishResp {
            outcome: WelcomePublishOutcome::Stored,
        }));
        r_pub_b.enqueue(DhtResponse::WelcomePublish(WelcomePublishResp {
            outcome: WelcomePublishOutcome::Stored,
        }));
        let (sid_a, addr_a) = spawn_fake_peer1(r_pub_a.clone()).await;
        let (sid_b, addr_b) = spawn_fake_peer1(r_pub_b.clone()).await;

        let r_self = ResponderState::new();
        let (sid_self, addr_self) = spawn_fake_peer1(r_self.clone()).await;

        let (client, our_ipk, _our_signer) =
            build_test_client_against(addr_self, sid_self);

        let recipient_ipk: [u8; 32] = [0xDE; 32];
        client.findnode_cache.lock().insert(
            welcome_routing_key(&recipient_ipk),
            FindNodeCacheEntry {
                descriptors: vec![
                    NodeDescriptor {
                        id:     sid_a,
                        addr:   addr_a,
                        pubkey: Bytes([0u8; 32]),
                    },
                    NodeDescriptor {
                        id:     sid_b,
                        addr:   addr_b,
                        pubkey: Bytes([0u8; 32]),
                    },
                ],
                cached_at:   Instant::now(),
            },
        );
        client.findnode_cache.lock().insert(
            welcome_routing_key(&our_ipk),
            FindNodeCacheEntry {
                descriptors: vec![NodeDescriptor {
                    id:     sid_self,
                    addr:   addr_self,
                    pubkey: Bytes([0u8; 32]),
                }],
                cached_at:   Instant::now(),
            },
        );

        let env = WelcomeEnvelopeP {
            version:       1,
            group_id:      [0u8; 32].into(),
            sender_ipk:    our_ipk.into(),
            recipient_ipk: recipient_ipk.into(),
            welcome_blob:  ByteVec(vec![0u8; 64]),
            kp_ref_used:   [0u8; 32].into(),
            sender_sig:    Bytes([0u8; 64]),
        };
        let outcome = client.publish_welcome_to_homes(&env).await.unwrap();
        assert_eq!(outcome, PublishOutcome::Stored);
        assert_eq!(r_pub_a.record_log().len(), 1);
        assert_eq!(r_pub_b.record_log().len(), 1);

        // Fetch path against the self home.
        let entry = WelcomeEntry {
            welcome_id: Bytes([0xAB; 8]),
            envelope:   env.clone(),
        };
        r_self.enqueue(DhtResponse::WelcomeFetch(WelcomeFetchResp {
            outcome: WelcomeFetchOutcome::Found(WelcomeFetchFound {
                welcomes: vec![entry.clone()],
            }),
        }));
        let entries = client.fetch_welcomes().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].welcome_id.0, [0xAB; 8]);

        // Ack path.
        r_self.enqueue(DhtResponse::WelcomeAck(WelcomeAckResp { ok: true }));
        client.ack_welcomes(&[entries[0].welcome_id.0]).await.unwrap();

        // Self-home: 1 fetch + 1 ack = 2.
        assert_eq!(r_self.record_log().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn find_k_closest_caches_within_ttl() {
        let responder = ResponderState::new();
        let (server_id, addr) = spawn_fake_peer1(responder.clone()).await;

        // First call: server returns 2 descriptors. Second call:
        // shouldn't even hit the server (cached).
        let descs = vec![NodeDescriptor {
            id:     NodeId::new([1u8; 32]),
            addr:   "127.0.0.1:1".parse().unwrap(),
            pubkey: Bytes([0u8; 32]),
        }];
        responder.enqueue(DhtResponse::FindNode(FindNodeResp {
            closer: descs.clone(),
        }));

        let (client, _our_ipk, _our_signer) =
            build_test_client_against(addr, server_id);

        let key = [0xAB; 32];
        let r1 = client.find_k_closest(key).await.unwrap();
        let r2 = client.find_k_closest(key).await.unwrap();
        assert_eq!(r1.len(), 1);
        assert_eq!(r2.len(), 1);
        // Only one FindNode RPC reached the server.
        let log = responder.record_log();
        let findnodes = log
            .iter()
            .filter(|r| matches!(r.req, DhtRequest::FindNode(_)))
            .count();
        assert_eq!(findnodes, 1, "second find_k_closest must hit the cache");
    }
}
