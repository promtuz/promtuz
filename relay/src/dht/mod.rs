//! Relay-side user-presence DHT.
//!
//! Top-level [`Dht`] struct wires the sub-systems together; the actual
//! routing-table / store / lookup / sync / publish logic lives in
//! sibling modules. Sub-lock layout follows §9.3 of the design doc.
//!
//! Phase 1a (this commit) ships the *shape* — module tree, struct
//! fields, public APIs, RocksDB column-family creation. Method bodies on
//! routing/lookup/store/etc. are stubbed so subsequent phases (1b–1h)
//! can drop in implementations without touching the wiring again.
//!
//! design-doc: `misc/specs/DHT.md`. Sections most relevant to this file:
//! §0 (constants), §1.2 (CFs), §1.3 (routing-table state), §9.1 (module
//! tree), §9.3 (Relay-struct sub-lock layout), §10 (Phase 1 = ship
//! feature-flagged off).

// Phase 1a is intentionally skeletal: the module tree, type signatures,
// and CF wiring exist but most bodies are `unimplemented!()`. Subsequent
// phases (1b–1h) populate them. Suppress dead-code warnings until those
// land — they're expected, and the noise drowns out real warnings.
#![allow(dead_code)]

// Original visibility was `pub(crate)`; the module-level pub-on-2-of-N
// items is the historical pattern from earlier phases (`pub mod config`,
// `pub mod metrics`). config + metrics stay `pub` because they're
// referenced from public types like `DhtConfig` in `Dht::new` (already
// re-exported below). Phase 5b bumps `handler` to `pub` so the e2e
// harness in `libcore/tests/e2e_phase5b.rs` can drive
// `handle_peer_connection` directly.
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

use anyhow::Context as _;
use anyhow::Result;
use common::quic::id::NodeId;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;
use rust_rocksdb::ColumnFamilyDescriptor;
use rust_rocksdb::DB as RocksDB;
use rust_rocksdb::Options;
use rust_rocksdb::SliceTransform;

use crate::quic::resolver_link::ResolverLinkHandle;

pub use config::DhtConfig;

use self::cache::LookupCache;
use self::metrics::Metrics;
use self::mls_kp::CF_DHT_KEYPACKAGE;
use self::mls_kp::KpFetchLimiters;
use self::mls_welcome::CF_DHT_WELCOME;
use self::mls_welcome::WelcomeLimiters;
use self::routing::RoutingTable;
use self::store::CF_DHT_MERKLE;
use self::store::CF_DHT_PRESENCE;
use self::store::CF_DHT_QUEUE;
use self::sync::MerkleState;

/// Top-level DHT runtime state.
///
/// Lock granularity matches §9.3:
/// - [`routing`] — `RwLock<RoutingTable>`, read-mostly.
/// - [`merkle`] — `RwLock<MerkleState>`, write-heavy.
/// - [`cache`] — `Mutex<LookupCache>` (a `Mutex` because every cache
///   touch is a write — even reads bump LRU recency once phase 1f
///   swaps in a real LRU).
/// - [`peer_conns`] — `RwLock<HashMap<NodeId, Connection>>`, mirroring
///   the existing `Relay::clients` pattern.
///
/// All sub-locks are `parking_lot` and *never* held across `await` —
/// callers clone what they need out of the lock first (cf. the
/// project-wide rule documented at
/// `relay/src/quic/handler/client/events/forward.rs:59`).
///
/// design-doc: §9.3.
///
/// `routing`/`merkle`/`cache`/`peer_conns` are `pub(crate)` because only
/// in-relay code holds an `Arc<Dht>`; `node_id`/`signing_key`/`cfg`/
/// `metrics` are `pub` so admin tools (`bin/ldb.rs` and friends) can read
/// without going through accessor stubs.
#[derive(Debug)]
pub struct Dht {
    /// 256-bucket routing table (§3.1).
    pub(crate) routing: RwLock<RoutingTable>,

    /// Shared `RocksDB` handle (the *same* DB the relay's message queue
    /// uses — column families, not databases, separate the two domains).
    /// `rust-rocksdb` is internally concurrency-safe with the
    /// `multi-threaded-cf` feature already declared in `relay/Cargo.toml`.
    ///
    /// CF handles are looked up via [`RocksDB::cf_handle`] at use-time
    /// rather than cached on the struct because the returned
    /// `Arc<BoundColumnFamily<'_>>` is lifetime-tied to the DB and
    /// can't be stored back as a struct field without painful self-refs.
    pub(crate) rocks: Arc<RocksDB>,

    /// Per-slice Merkle anti-entropy state (§6).
    pub(crate) merkle: RwLock<MerkleState>,

    /// `(target_ipk → relay_descriptor)` cache for repeat lookups (§4.4).
    pub(crate) cache: Mutex<LookupCache>,

    /// Hot relay-to-relay connections, keyed by remote `NodeId`. Strong
    /// reference held here; routing-table entries hold a `Weak` (§1.3).
    ///
    /// **Phase 1h, item 1 — verified TLS pubkey caching.** The value is a
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

    /// Resolver session handle for the bootstrap-retry path (phase 1h
    /// item 5). Wired in after `Dht::new` via [`Self::attach_resolver`]
    /// because `ResolverLinkHandle` is a relay-side type that can't
    /// be passed through the constructor without a circular import
    /// (this module is reachable from `relay/src/relay/mod.rs`).
    ///
    /// `None` in unit-test fixtures (`fresh_dht`) and in legacy
    /// configs where the DHT runs without a resolver link. The
    /// scheduler's bootstrap-retry branch checks `Option` and degrades
    /// to "log the warning, do nothing" when absent.
    pub(crate) resolver: parking_lot::RwLock<Option<ResolverLinkHandle>>,

    /// This relay's NodeId — `BLAKE3(NodeKey)` (§0).
    pub node_id: NodeId,

    /// This relay's identity signing key. Used to sign `relay_sig` on
    /// every outgoing `PresenceRecord` (§1.1.1) and tombstones.
    /// Distinct from the TLS server key (`Relay::keys::signing` is the
    /// one and only identity key — see the doc comment on
    /// `relay/src/relay/mod.rs::RelayKeys`).
    pub signing_key: SigningKey,

    /// Local copy of the runtime config so DHT code paths don't have to
    /// reach back into `Relay::cfg`.
    pub cfg: DhtConfig,

    /// Aggregate operation counters (§9.1).
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

    /// Per-peer inbound-RPC rate limiters (phase 1h item 2). One
    /// `governor::RateLimiter` per RPC class (cheap / expensive /
    /// bulk), keyed on the requester's `NodeId`. The default keyed
    /// state store evicts idle peers automatically so a churn-heavy
    /// workload doesn't grow the limiters unboundedly. Shared
    /// reference because the dispatcher in `handler.rs` checks the
    /// limiter on every inbound stream.
    pub(crate) rate_limiters: rate_limit::PerPeerLimiters,

    /// MLS Phase 2 — per-`(target_ipk, requester_relay_id)` rate
    /// limiter for `KeyPackageFetch` (`MAX_KP_FETCH_PER_HOUR = 60`).
    /// Distinct from [`Self::rate_limiters`] (which is keyed on the
    /// requester alone, the coarse first-line bulkhead) because the
    /// §5.6 anti-pinning policy demands per-pair attribution: a
    /// misbehaving relay draining Bob's stash must not freeze its
    /// quota for legitimate fetches against Alice's stash.
    ///
    /// design-doc: `misc/specs/MLS.md` §5.6.
    pub(crate) kp_fetch_limiters: KpFetchLimiters,

    /// MLS Phase 3a — per-relay rate limiter for the
    /// `WelcomePublish` / `WelcomeFetch` / `WelcomeAck` family. The
    /// welcome RPCs are classified `Bulk` in
    /// [`Self::rate_limiters`] (the coarse first-line bulkhead);
    /// this dedicated limiter adds a welcome-specific quota so a
    /// peer that's well under the per-relay bulk quota cannot still
    /// pin a single recipient's welcome queue. Mirrors the
    /// [`Self::kp_fetch_limiters`] pattern.
    ///
    /// design-doc: `misc/specs/MLS.md` §6.1; Phase 3a Component B.
    pub(crate) welcome_limiters: WelcomeLimiters,

    /// Shared reference to the relay's connected-clients map (phase 2d).
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

/// Shared reference to the relay's connected-clients map (phase 2d).
/// Aliased so the field type stays readable. See `Dht::clients` for
/// the lock contract and the `Option<...>` rationale.
pub(crate) type ClientsMap = Arc<RwLock<HashMap<[u8; 32], Connection>>>;

impl Dht {
    /// Construct the runtime DHT state.
    ///
    /// Idempotently opens the `dht_presence` and `dht_merkle` column
    /// families on the supplied `RocksDB` path. The same DB instance the
    /// caller already opened for the message queue is reused; CFs
    /// separate the two key-spaces (§1.2). "Already exists" is **not**
    /// an error — the relay-restart case is exactly that.
    ///
    /// **Important:** `rocks` must already have been opened with the
    /// `dht_presence` and `dht_merkle` CFs declared. We don't take an
    /// owned `DB` handle and reopen it because the message-queue side
    /// is already using the same handle and reopening would invalidate
    /// outstanding iterators. See `crate::util::rocksdb::rocksdb` —
    /// in this phase we extend that to declare the CFs up front, so
    /// `Dht::new` only verifies the CFs are present and stashes the
    /// shared `Arc<DB>`.
    pub fn new(
        node_id: NodeId, signing_key: SigningKey, cfg: DhtConfig, rocks: Arc<RocksDB>,
    ) -> Result<Self> {
        // Verify the required CFs were declared at DB-open time. If the
        // relay was started against an old DB without the CFs, surface
        // a clear error rather than panicking deep inside a put.
        rocks
            .cf_handle(CF_DHT_PRESENCE)
            .with_context(|| format!("missing column family `{CF_DHT_PRESENCE}` in DB"))?;
        rocks
            .cf_handle(CF_DHT_MERKLE)
            .with_context(|| format!("missing column family `{CF_DHT_MERKLE}` in DB"))?;
        // Sticky-home queue CF (phase 2a). Idempotent on a fresh DB
        // because `crate::util::rocksdb` opens with
        // `create_missing_column_families(true)`. On an upgrade against
        // an existing DB, the CF is auto-created at first open by the
        // same flag — `Dht::new` only re-verifies the handle exists.
        rocks
            .cf_handle(CF_DHT_QUEUE)
            .with_context(|| format!("missing column family `{CF_DHT_QUEUE}` in DB"))?;
        // MLS Phase 2 KeyPackage stash CF.
        rocks
            .cf_handle(CF_DHT_KEYPACKAGE)
            .with_context(|| format!("missing column family `{CF_DHT_KEYPACKAGE}` in DB"))?;
        // MLS Phase 3a welcome queue CF.
        rocks
            .cf_handle(CF_DHT_WELCOME)
            .with_context(|| format!("missing column family `{CF_DHT_WELCOME}` in DB"))?;

        Ok(Self {
            routing: RwLock::new(RoutingTable::empty(node_id)),
            rocks,
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

    /// Wire a shared reference to the relay's connected-clients map
    /// (phase 2d). Called by `Relay::new` *after* both the `Relay` and
    /// the `Dht` are constructed, so the same `Arc<RwLock<HashMap<...>>>`
    /// the relay-side hands to the per-client handler is also visible
    /// to the home-side `Forward` handler.
    ///
    /// Idempotent and safe to skip in unit-test fixtures that don't
    /// drive the home-side delivery path.
    pub fn attach_clients(&mut self, clients: Arc<RwLock<HashMap<[u8; 32], Connection>>>) {
        self.clients = Some(clients);
    }

    /// Phase 5b: pre-seed a peer descriptor into this relay's routing
    /// table. Used by the e2e harness (`libcore/tests/e2e_phase5b.rs`)
    /// to wire N relays' routing tables to each other before the test
    /// fires its first `FindNode` RPC. Production paths populate the
    /// table organically via `handle_peer_connection` post-DhtHello.
    pub fn seed_routing_table(&self, descriptor: common::proto::dht_p2p::NodeDescriptor) {
        let _ = self.routing.write().insert(descriptor);
    }

    /// Phase 5b: purge any routing-table entry whose `id` is not in
    /// `allowed`. The e2e harness uses this to evict libcore-ephemeral
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

    /// Wire the resolver-session handle for the bootstrap-retry path
    /// (phase 1h item 5). Called by `relay/src/main.rs` after
    /// `ResolverLink::new` produces its `client_handle`.
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
    /// design-doc: §7.1 (conn-close watcher). Symmetric to the resolver's
    /// `Resolver::close` (`resolver/src/resolver/mod.rs`).
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

/// Helper: the DHT-specific column family descriptors to pass into
/// `RocksDB::open_cf_descriptors` at relay-startup time.
///
/// Used by `crate::util::rocksdb` so DB-open and DHT-init aren't two
/// places that have to stay in sync about which CFs exist.
///
/// design-doc: `misc/specs/DHT.md` §1.2 (presence + merkle CFs);
/// `misc/specs/STICKY_HOME_RELAY.md` §6.1 (queue CF);
/// `misc/specs/MLS.md` §2.5 (`cf_dht_keypackage`).
pub fn dht_cf_descriptors() -> Vec<ColumnFamilyDescriptor> {
    // - `dht_presence`: no prefix extractor — point lookups only on
    //   32-byte keys (§1.2 trade-off note).
    // - `dht_merkle`: no prefix extractor — keys are 3 bytes
    //   (slice/level/index) which would be malformed under any
    //   non-trivial extractor.
    // - `dht_queue`: 32-byte fixed prefix extractor matching the
    //   `recipient` field at offset 0 in `crate::storage::MessageKey`.
    //   Same shape as the relay's existing message-queue (default CF)
    //   so the per-recipient drain iterator works the same way.
    // - `dht_keypackage`: 32-byte fixed prefix extractor matching the
    //   `stash_prefix(ipk) = BLAKE3("kp:" || ipk)` field at offset 0
    //   in the `(stash_prefix(32) || kp_ref(32))` storage key.
    //   Layout per `mls_kp::stash_prefix`.
    let presence_opts = Options::default();
    let merkle_opts = Options::default();
    let mut queue_opts = Options::default();
    queue_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));
    let mut keypackage_opts = Options::default();
    keypackage_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));
    let mut welcome_opts = Options::default();
    welcome_opts.set_prefix_extractor(SliceTransform::create_fixed_prefix(32));
    vec![
        ColumnFamilyDescriptor::new(CF_DHT_PRESENCE, presence_opts),
        ColumnFamilyDescriptor::new(CF_DHT_MERKLE, merkle_opts),
        ColumnFamilyDescriptor::new(CF_DHT_QUEUE, queue_opts),
        ColumnFamilyDescriptor::new(CF_DHT_KEYPACKAGE, keypackage_opts),
        ColumnFamilyDescriptor::new(CF_DHT_WELCOME, welcome_opts),
    ]
}
