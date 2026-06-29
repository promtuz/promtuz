//! Relay-side user-presence DHT.
//!
//! Top-level [`Dht`] struct wires the sub-systems together; the actual
//! routing-table / store / lookup / sync / publish logic lives in
//! sibling modules.

// Some routing/lookup/store helpers are only reachable from code paths
// that aren't always compiled in; suppress dead-code warnings so the
// noise doesn't drown out real warnings.
#![allow(dead_code)]

// config + metrics are `pub` because they're referenced from public
// types like `DhtConfig` in `Dht::new` (already re-exported below).
// `handler` is `pub` so the e2e harness in
// `libcore/tests/e2e_phase5b.rs` can drive `handle_peer_connection`
// directly.
pub(crate) mod bootstrap;
pub(crate) mod cache;
pub mod config;
pub(crate) mod forward;
pub mod handler;
pub(crate) mod key_helpers;
pub(crate) mod lookup;
pub mod metrics;
pub(crate) mod mls_fanout;
pub(crate) mod mls_kp;
pub(crate) mod mls_kp_originate;
pub(crate) mod mls_welcome;
pub(crate) mod mls_welcome_originate;
pub(crate) mod peer_dial;
pub(crate) mod publish;
pub(crate) mod queue_drain;
pub(crate) mod rate_limit;
pub(crate) mod routing;
pub(crate) mod store;
pub(crate) mod sync;
pub(crate) mod tls_extract;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use common::quic::id::NodeId;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;

use crate::quic::resolver_link::ResolverLinkHandle;
use crate::storage::db::Store;

pub use config::DhtConfig;

use self::cache::LookupCache;
use self::metrics::Metrics;
use self::mls_kp::KpFetchLimiters;
use self::mls_welcome::WelcomeLimiters;
use self::routing::RoutingTable;
use self::sync::MerkleState;

/// Top-level DHT runtime state.
///
/// Lock granularity:
/// - [`routing`] — `RwLock<RoutingTable>`, read-mostly.
/// - [`merkle`] — `RwLock<MerkleState>`, write-heavy.
/// - [`cache`] — `Mutex<LookupCache>` (a `Mutex` because every cache
///   touch is a write — even reads bump LRU recency).
/// - [`peer_conns`] — `RwLock<HashMap<NodeId, Connection>>`, mirroring
///   the existing `Relay::clients` pattern.
///
/// All sub-locks are `parking_lot` and *never* held across `await` —
/// callers clone what they need out of the lock first (cf. the
/// project-wide rule documented at
/// `relay/src/quic/handler/client/events/forward.rs:59`).
///
/// `routing`/`merkle`/`cache`/`peer_conns` are `pub(crate)` because only
/// in-relay code holds an `Arc<Dht>`; `node_id`/`signing_key`/`cfg`/
/// `metrics` are `pub` so admin tools (`bin/ldb.rs` and friends) can read
/// without going through accessor stubs.
#[derive(Debug)]
pub struct Dht {
    /// 256-bucket routing table.
    pub(crate) routing: RwLock<RoutingTable>,

    /// Shared fjall store (the *same* store the relay's message queue uses —
    /// keyspaces, not databases, separate the two domains). Keyspace handles
    /// are cheap `Clone`s held on [`Store`], so DHT code reaches them as
    /// `dht.store.presence` / `.queue` / `.keypackage` / `.welcome` directly.
    pub(crate) store: Arc<Store>,

    /// Per-slice Merkle anti-entropy state.
    pub(crate) merkle: RwLock<MerkleState>,

    /// `(target_ipk → relay_descriptor)` cache for repeat lookups.
    pub(crate) cache: Mutex<LookupCache>,

    /// Hot relay-to-relay connections, keyed by remote `NodeId`. Strong
    /// reference held here; routing-table entries hold a `Weak`.
    ///
    /// **Verified TLS pubkey caching.** The value is a
    /// `(Connection, [u8; 32])` tuple where the second element is the
    /// peer's Ed25519 cert SPKI extracted post-handshake from
    /// `connection.peer_identity()`. The relay-side TLS verifier ALSO
    /// checks `BLAKE3(spki) == claimed_node_id` (defense-in-depth) at
    /// dial time; a mismatch closes the connection with
    /// `CloseReason::DhtMalformedKey` before the entry is cached. See
    /// `dht::lookup::connect_to_peer` and `dht::handler::handle_peer_connection`
    /// for the two callsite paths.
    ///
    /// Inbound (server-side) entries do *not* populate the pubkey
    /// because the relay's QUIC server config currently uses
    /// `with_no_client_auth()` — clients (peer dialers) do not present
    /// certs during the handshake, so `peer_identity()` returns `None`.
    /// In that case we cache the connection with `[0u8; 32]` and rely
    /// on outbound dials (and `NodeDescriptor.pubkey` in
    /// `FindNode`/`FindValue` responses) to backfill the verified
    /// pubkey before any cert-pinning consumer reads it. This gap is
    /// documented as a follow-up; closing it requires enabling mTLS
    /// on `peer/1`, which lives in `common/src/quic/config.rs` and
    /// is out of scope for this dispatch.
    pub(crate) peer_conns: RwLock<HashMap<NodeId, (Connection, [u8; 32])>>,

    /// Resolver session handle for the bootstrap-retry path. Wired in
    /// after `Dht::new` via [`Self::attach_resolver`] because
    /// `ResolverLinkHandle` is a relay-side type that can't
    /// be passed through the constructor without a circular import
    /// (this module is reachable from `relay/src/relay/mod.rs`).
    ///
    /// `None` in unit-test fixtures (`fresh_dht`) and in legacy
    /// configs where the DHT runs without a resolver link. The
    /// scheduler's bootstrap-retry branch checks `Option` and degrades
    /// to "log the warning, do nothing" when absent.
    pub(crate) resolver: parking_lot::RwLock<Option<ResolverLinkHandle>>,

    /// This relay's NodeId — `BLAKE3(NodeKey)`.
    pub node_id: NodeId,

    /// This relay's identity signing key. Used to sign `relay_sig` on
    /// every outgoing `PresenceRecord` and tombstones. Distinct from the
    /// TLS server key (`Relay::keys::signing` is the one and only identity
    /// key — see `relay/src/relay/mod.rs::RelayKeys`).
    pub signing_key: SigningKey,

    /// Local copy of the runtime config so DHT code paths don't have to
    /// reach back into `Relay::cfg`.
    pub cfg: DhtConfig,

    /// Aggregate operation counters.
    pub metrics: Metrics,

    /// QUIC endpoint we use to dial outbound peer connections. Cloned
    /// from `Relay::endpoint` at construction. `Option` because the unit
    /// tests in `store.rs` / `lookup.rs` build `Dht`s without a live
    /// endpoint (they only exercise local-only code paths).
    pub(crate) endpoint: Option<Endpoint>,

    /// `peer/1` ALPN client config — used by `lookup.rs::connect_to_peer`
    /// to dial outbound DHT peer connections. Same `Option` rationale
    /// as [`endpoint`].
    pub(crate) peer_client_cfg: Option<Arc<ClientConfig>>,

    /// Per-peer inbound-RPC rate limiters. One
    /// `governor::RateLimiter` per RPC class (cheap / expensive /
    /// bulk), keyed on the requester's `NodeId`. The default keyed
    /// state store evicts idle peers automatically so a churn-heavy
    /// workload doesn't grow the limiters unboundedly. Shared
    /// reference because the dispatcher in `handler.rs` checks the
    /// limiter on every inbound stream.
    pub(crate) rate_limiters: rate_limit::PerPeerLimiters,

    /// Per-`(target_ipk, requester_relay_id)` rate limiter for
    /// `KeyPackageFetch` (`MAX_KP_FETCH_PER_HOUR = 60`).
    /// Distinct from [`Self::rate_limiters`] (which is keyed on the
    /// requester alone, the coarse first-line bulkhead) because the
    /// anti-pinning policy demands per-pair attribution: a misbehaving
    /// relay draining Bob's stash must not freeze its quota for
    /// legitimate fetches against Alice's stash.
    pub(crate) kp_fetch_limiters: KpFetchLimiters,

    /// Per-relay rate limiter for the
    /// `WelcomePublish` / `WelcomeFetch` / `WelcomeAck` family. The
    /// welcome RPCs are classified `Bulk` in [`Self::rate_limiters`]
    /// (the coarse first-line bulkhead); this dedicated limiter adds a
    /// welcome-specific quota so a peer that's well under the per-relay
    /// bulk quota cannot still pin a single recipient's welcome queue.
    /// Mirrors the [`Self::kp_fetch_limiters`] pattern.
    pub(crate) welcome_limiters: WelcomeLimiters,

    /// Shared reference to the relay's connected-clients map.
    ///
    /// The home-side `Forward` handler in
    /// [`crate::dht::forward::handle_forward_rpc`] uses this to short-
    /// circuit to local-deliver when the recipient is currently
    /// authenticated *here* — avoiding a write into `cf_dht_queue` that
    /// would just be drained moments later when the user pulls. The
    /// `Connection` value is exactly the one `RelayRef::clients` holds;
    /// we share an `Arc<RwLock<...>>` clone rather than a back-pointer
    /// to `Relay` so unit tests that build a bare `Dht` (without a full
    /// `Relay`) can still drive the handler with a stubbed map.
    ///
    /// **Lock contract**: `parking_lot::RwLock`; never held across an
    /// `await` (project-wide rule). Callers clone the `Connection` out
    /// of the guard before any I/O — same pattern
    /// `quic/handler/client/events/forward.rs::handle_forward` uses on
    /// the sender path.
    ///
    /// `Option<...>` so the bare-`Dht` unit-test fixtures
    /// (`store::tests::fresh_dht`, `lookup::tests::fresh_dht`) keep
    /// compiling — a relay-level `Relay::new` populates this via
    /// [`Self::attach_clients`].
    pub(crate) clients: Option<ClientsMap>,
}

/// Shared reference to the relay's connected-clients map. Aliased so
/// the field type stays readable. See `Dht::clients` for the lock
/// contract and the `Option<...>` rationale.
pub(crate) type ClientsMap = Arc<RwLock<HashMap<[u8; 32], Connection>>>;

impl Dht {
    /// Construct the runtime DHT state over the shared fjall [`Store`].
    ///
    /// The same store the relay opened for its message queue is reused;
    /// keyspaces (`dht_presence` / `dht_queue` / `dht_keypackage` /
    /// `dht_welcome`) separate the two domains and are all opened up front in
    /// [`Store::open`], so there is nothing to verify here. Returns `Result`
    /// only to keep the call sites stable.
    pub fn new(
        node_id: NodeId, signing_key: SigningKey, cfg: DhtConfig, store: Arc<Store>,
    ) -> Result<Self> {
        Ok(Self {
            routing: RwLock::new(RoutingTable::empty(node_id)),
            store,
            merkle: RwLock::new(MerkleState::empty()),
            cache: Mutex::new(LookupCache::empty()),
            peer_conns: RwLock::new(HashMap::new()),
            resolver: parking_lot::RwLock::new(None),
            node_id,
            signing_key,
            cfg,
            metrics: Metrics::new(),
            endpoint: None,
            peer_client_cfg: None,
            rate_limiters: rate_limit::PerPeerLimiters::new(),
            kp_fetch_limiters: KpFetchLimiters::new(),
            welcome_limiters: WelcomeLimiters::new(),
            clients: None,
        })
    }

    /// Wire the outbound-dial machinery in. Called by `Relay::new` after
    /// the QUIC endpoint and per-role client configs have been built.
    /// Split from `Dht::new` so unit tests that only need DB-level state
    /// don't have to construct a full QUIC stack.
    pub fn attach_dialer(&mut self, endpoint: Endpoint, peer_client_cfg: Arc<ClientConfig>) {
        self.endpoint = Some(endpoint);
        self.peer_client_cfg = Some(peer_client_cfg);
    }

    /// Wire a shared reference to the relay's connected-clients map.
    /// Called by `Relay::new` *after* both the `Relay` and the `Dht`
    /// are constructed, so the same `Arc<RwLock<HashMap<...>>>`
    /// the relay-side hands to the per-client handler is also visible
    /// to the home-side `Forward` handler.
    ///
    /// Idempotent and safe to skip in unit-test fixtures that don't
    /// drive the home-side delivery path.
    pub fn attach_clients(&mut self, clients: Arc<RwLock<HashMap<[u8; 32], Connection>>>) {
        self.clients = Some(clients);
    }

    /// Pre-seed a peer descriptor into this relay's routing table. Used
    /// by the e2e harness (`libcore/tests/e2e_phase5b.rs`) to wire N
    /// relays' routing tables to each other before the test fires its
    /// first `FindNode` RPC. Production paths populate the table
    /// organically via `handle_peer_connection` post-DhtHello.
    pub fn seed_routing_table(&self, descriptor: common::proto::dht_p2p::NodeDescriptor) {
        let _ = self.routing.write().insert(descriptor);
    }

    /// Purge any routing-table entry whose `id` is not in `allowed`.
    /// The e2e harness uses this to evict libcore-ephemeral
    /// peers from the routing table after each operation that may have
    /// added them, so subsequent `FindNode`s return only the curated
    /// cross-wired set.
    ///
    /// Production code never calls this — it'd undermine the
    /// learn-from-traffic policy.
    pub fn purge_routing_to(&self, allowed: &[common::quic::id::NodeId]) {
        let allowed: std::collections::HashSet<common::quic::id::NodeId> =
            allowed.iter().copied().collect();
        let mut rt = self.routing.write();
        for bucket in rt.buckets.iter_mut() {
            bucket.entries.retain(|e| allowed.contains(&e.id));
            bucket.candidates.retain(|e| allowed.contains(&e.id));
        }
    }

    /// Wire the resolver-session handle for the bootstrap-retry path.
    /// Called by `relay/src/main.rs` after `ResolverLink::new` produces
    /// its `client_handle`.
    ///
    /// Idempotent: a second call replaces the cached handle. The
    /// scheduler reads `dht.resolver.read().clone()` on each tick so
    /// the swap is visible to the next bootstrap-retry attempt.
    ///
    /// Takes `&self` (not `&mut self`) so the call-site can pass an
    /// `Arc<Dht>` without unwrapping — the field is interior-mutable
    /// behind a `parking_lot::RwLock`.
    pub fn attach_resolver(&self, handle: ResolverLinkHandle) {
        *self.resolver.write() = Some(handle);
    }

    /// Close every cached peer connection and clear the map. Called by
    /// the `Relay`-level shutdown handler so in-flight DHT RPCs cleanly
    /// finish before the QUIC endpoint is torn down.
    ///
    /// Symmetric to the resolver's `Resolver::close`
    /// (`resolver/src/resolver/mod.rs`).
    pub async fn shutdown(&self) {
        use common::quic::CloseReason;
        // Drain the map first so we don't hold the write lock across the
        // (synchronous, but still capability-effecting) close calls.
        let conns: Vec<Connection> = {
            let mut guard = self.peer_conns.write();
            guard.drain().map(|(_, (c, _pk))| c).collect()
        };
        for conn in conns {
            CloseReason::ShuttingDown.close(&conn);
            self.metrics.inc_peer_conns_closed();
        }
    }
}
