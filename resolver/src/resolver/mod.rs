use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::proto::RelayId;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::gateway_hello_signing_input;
use common::proto::relay_res::relay_heartbeat_signing_input;
use common::proto::relay_res::relay_hello_signing_input;
use common::quic::CloseReason;
use common::quic::config::build_server_cfg;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeId;
use common::quic::id::NodeKey;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use common::warn;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use parking_lot::RwLock;
use quinn::Connection;
use quinn::Endpoint;
use quinn::ServerConfig;

use crate::resolver::relays::RelayEntry;
use crate::util::config::AppConfig;
use crate::util::systime;

pub mod relays;
pub mod rpc;

/// Resolver is purely shared, immutable scaffolding plus the interior-mutable
/// `relays` registry. Cloning the [`Arc`] is cheap and lock-free; only the
/// registry itself takes a (parking_lot) lock when read or written.
pub type ResolverRef = Arc<Resolver>;

/// Maximum simultaneous registrations. Past this we reject incoming
/// `RelayHello`s with [`CloseReason::RegistryFull`] — keeps an attacker who
/// holds valid identity keys from blowing up unbounded memory by churning
/// registrations from many spoofed-but-real relays.
const MAX_RELAYS: usize = 1024;

/// Max simultaneously-registered gateways. Gateways are project infra, so this
/// is a generous ceiling, not an expected count.
const MAX_GATEWAYS: usize = 64;

/// Maximum permitted clock skew between the relay's signed `timestamp` and
/// our local clock, in milliseconds. Anything outside this window is treated
/// as a replay (or a misconfigured clock) and rejected.
const HELLO_MAX_SKEW_MS: u128 = 60_000;

/// Represents a single resolver node in the network but locally
///
/// contains all necessary information instead of a global state
#[derive(Debug)]
pub struct Resolver {
    /// Long-term identity / verification key. Currently unread but kept here
    /// so the planned resolver-to-resolver gossip path (see
    /// [`crate::quic::handler::resolver`]) can sign outbound packets without
    /// re-reading the on-disk PKCS#8 file. Remove this `allow` once the
    /// gossip layer actually consumes it.
    #[allow(dead_code)]
    pub key: NodeKey,
    /// Held for the same reason as [`Self::key`] — the gossip layer will
    /// need access to peer-resolver seed addresses, TLS roots, etc.
    #[allow(dead_code)]
    pub cfg: AppConfig,
    pub endpoint: Arc<Endpoint>,
    /// Live relay registry. Read-mostly: every client `GetRelays` RPC reads
    /// it, only registration/eviction writes. Hence `RwLock` rather than
    /// `Mutex`.
    relays: RwLock<HashMap<RelayId, RelayEntry>>,

    /// Registered push gateways. Same entry type as relays; a plain directory
    /// — the resolver can't see the gateway's capability cert (no client
    /// auth), so a relay verifies `PUSH_GATEWAY` when it dials the gateway.
    gateways: RwLock<HashMap<RelayId, RelayEntry>>,
}

impl Resolver {
    fn get_server_cfg(cfg: &AppConfig) -> Result<ServerConfig> {
        setup_crypto_provider()?;
        use ProtoRole as PR;
        build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &[PR::Resolver, PR::Relay, PR::Client],
        )
    }

    fn key(cfg: &AppConfig) -> NodeKey {
        // `secret_from_key` returns `Result<_, ()>` and logs its own
        // detailed reason on the error path, so we just convert `()` into
        // a placeholder string for `graceful!`'s log line.
        let secret = graceful!(
            secret_from_key(&cfg.network.key_path).map_err(|_| "see prior error"),
            "loading the resolver key"
        );

        graceful!(NodeKey::new(secret.verifying_key()), "deriving the resolver node id")
    }

    fn endpoint(cfg: &AppConfig) -> Endpoint {
        let server_config = graceful!(Self::get_server_cfg(cfg), "building the TLS server config");
        let endpoint = graceful!(
            Endpoint::server(server_config, cfg.network.bind_addr()),
            "starting the QUIC endpoint"
        );

        if let Ok(addr) = endpoint.local_addr() {
            info!("resolver listening at QUIC({:?})", addr);
        }

        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let key = Self::key(&cfg);

        info!("initializing resolver with IPK({})", key.key());

        Self {
            key,
            endpoint: Arc::new(Self::endpoint(&cfg)),
            relays: RwLock::new(HashMap::new()),
            gateways: RwLock::new(HashMap::new()),
            cfg,
        }
    }

    /// Authenticate and admit a relay registration.
    ///
    /// **Auth model:** the wire `RelayHello` carries the relay's full
    /// Ed25519 pubkey alongside the derived `relay_id`. We:
    ///
    /// 1. Verify `relay_id == BLAKE3(pubkey)` (binds id to pubkey).
    /// 2. Verify the Ed25519 signature over the canonical transcript
    ///    (binds the wire packet to the holder of the secret key).
    /// 3. Verify `timestamp` is within ±`HELLO_MAX_SKEW_MS` of local time
    ///    (replay protection — captured packets stop being usable
    ///    quickly).
    /// 4. Enforce the `MAX_RELAYS` capacity cap before any mutation.
    /// 5. Last-connection-wins: if a live entry already exists for this id,
    ///    close it and let the fresh (signature-proven) registration take
    ///    over. A relay restart/redeploy leaves its old QUIC conn lingering
    ///    (no FIN → `close_reason` stays `None`); rejecting locked the relay
    ///    out until that idle-timed out and made it hammer the per-IP rate
    ///    limit. The freshness window + client-side relay-cert pinning bound
    ///    the replay-flap risk (a redirected addr just fails closed).
    /// 6. Insert the new entry under the write lock.
    ///
    /// Why not derive the pubkey from `relay_id` alone? Because `relay_id`
    /// is a BLAKE3 hash (see `common::quic::id::NodeKey`) and is therefore
    /// not invertible. Carrying the full pubkey on the wire is the minimal
    /// coherent option — the alternative would be a challenge/response
    /// round-trip, which adds latency for no extra security against this
    /// threat model.
    pub fn register_relay(
        &self, conn: Arc<Connection>, hello: &LifetimeP,
    ) -> Result<LifetimeP, CloseReason> {
        let LifetimeP::RelayHello { relay_id, pubkey, timestamp, sig } = hello else {
            return Err(CloseReason::PacketMismatch);
        };
        let (relay_id, timestamp) = (*relay_id, *timestamp);

        // 1-3. shared id-binding + signature + freshness check.
        let msg = relay_hello_signing_input(&relay_id, &pubkey.0, timestamp);
        verify_signed_packet(
            conn.remote_address(),
            "hello",
            &relay_id,
            &pubkey.0,
            &sig.0,
            &msg,
            timestamp,
        )?;

        let now = systime().as_millis();

        // 4-6. registry mutation under write lock
        let mut relays = self.relays.write();

        // 5. Last-connection-wins (see doc): close any superseded live entry
        // and let this fresh, signature-proven registration replace it. The
        // ptr-guarded watcher (`remove_relay_if_same`) makes the displaced
        // connection's cleanup a no-op, so it can't evict the new entry.
        if let Some(existing) = relays.get(&relay_id) {
            if existing.conn.close_reason().is_none() {
                info!("relay({relay_id}) reconnected, superseding prior session");
                CloseReason::Reconnecting.close(&existing.conn);
            }
            relays.remove(&relay_id);
        }

        // 4. capacity cap (after pruning a known-stale slot above)
        if relays.len() >= MAX_RELAYS {
            warn!(
                "relay({}) rejected: registry full ({}/{})",
                conn.remote_address(),
                relays.len(),
                MAX_RELAYS
            );
            return Err(CloseReason::RegistryFull);
        }

        // `RelayEntry::new` stamps `last_heartbeat_at = Instant::now()`
        // so a `Hello` immediately followed by `GetBootstrapPeers` ranks
        // this just-joined relay as fresh under the `rtt_near` proxy.
        // `pubkey` is captured here so subsequent `GetBootstrapPeers`
        // responses can include it without re-deriving from cert state.
        relays.insert(relay_id, RelayEntry::new(relay_id, conn, *pubkey));

        let hello_ack = LifetimeP::HelloAck { resolver_time: now };

        Ok(hello_ack)
    }

    /// Spawn a per-registration watcher that removes the relay entry once
    /// its connection closes — but only if the live entry is still the
    /// same `Arc<Connection>` we registered. This avoids the obvious race
    /// where a fresh re-registration arrives between the old `closed()`
    /// firing and us taking the write lock.
    pub fn watch_relay(self: &Arc<Self>, relay_id: RelayId, conn: Arc<Connection>) {
        let resolver = self.clone();
        tokio::spawn(async move {
            let _ = conn.closed().await;
            resolver.remove_relay_if_same(relay_id, &conn);
        });
    }

    fn remove_relay_if_same(&self, relay_id: RelayId, conn: &Arc<Connection>) {
        let mut relays = self.relays.write();
        let same = relays
            .get(&relay_id)
            .map(|e| Arc::ptr_eq(&e.conn, conn))
            .unwrap_or(false);
        if same {
            relays.remove(&relay_id);
        }
    }

    /// Snapshot of the current registry, intended for serving client
    /// `GetRelays` queries. Holds the read lock only for the duration of
    /// the clone — never across an `await`.
    pub fn snapshot_relays(&self) -> Vec<RelayEntry> {
        self.relays.read().values().cloned().collect()
    }

    /// Authenticate an inbound [`LifetimeP::RelayHeartbeat`].
    ///
    /// Mirrors `register_relay`'s id-binding + signature + freshness checks.
    /// Heartbeats need their own signature (not just connection-bound trust)
    /// so future liveness/load-aware routing logic can't be poisoned by a
    /// peer that knew a registered relay's `relay_id`.
    ///
    /// Additionally requires that the `relay_id` is currently registered:
    /// a heartbeat for an unknown relay is meaningless and is rejected.
    pub fn verify_heartbeat(
        &self, conn: &Connection, packet: &LifetimeP,
    ) -> Result<(), CloseReason> {
        let LifetimeP::RelayHeartbeat { relay_id, pubkey, timestamp, sig, .. } = packet else {
            return Err(CloseReason::PacketMismatch);
        };
        let (relay_id, timestamp) = (*relay_id, *timestamp);

        let msg = relay_heartbeat_signing_input(&relay_id, &pubkey.0, timestamp);
        verify_signed_packet(
            conn.remote_address(),
            "heartbeat",
            &relay_id,
            &pubkey.0,
            &sig.0,
            &msg,
            timestamp,
        )?;

        // The signature is valid for *some* relay; require that it's the
        // one currently using this connection. Holding the read lock only
        // for the lookup keeps this lock-cheap.
        //
        // Bumps the entry's `last_heartbeat_at` while the lookup is still
        // in scope: the per-entry `Mutex` lets us update one entry's
        // recency without escalating the outer registry `RwLock` to a
        // write lock (which would serialise every other reader).
        match self.relays.read().get(&relay_id) {
            None => {
                warn!(
                    "relay({}) heartbeat rejected: relay_id {} not registered",
                    conn.remote_address(),
                    relay_id
                );
                return Err(CloseReason::PacketMismatch);
            },
            Some(entry) => entry.touch_heartbeat(std::time::Instant::now()),
        }

        Ok(())
    }

    /// Admit a gateway registration. Mirrors [`Self::register_relay`]:
    /// id↔pubkey binding + signature + freshness, then last-connection-wins.
    /// The `PUSH_GATEWAY` capability is deliberately NOT checked here — the
    /// resolver never sees the gateway's cert; a relay verifies it at dial.
    pub fn register_gateway(
        &self, conn: Arc<Connection>, hello: &LifetimeP,
    ) -> Result<LifetimeP, CloseReason> {
        let LifetimeP::GatewayHello { gateway_id, pubkey, timestamp, sig } = hello else {
            return Err(CloseReason::PacketMismatch);
        };
        let (gateway_id, timestamp) = (*gateway_id, *timestamp);

        let msg = gateway_hello_signing_input(&gateway_id, &pubkey.0, timestamp);
        verify_signed_packet(
            conn.remote_address(),
            "gateway-hello",
            &gateway_id,
            &pubkey.0,
            &sig.0,
            &msg,
            timestamp,
        )?;

        let now = systime().as_millis();
        let mut gateways = self.gateways.write();

        if let Some(existing) = gateways.get(&gateway_id) {
            if existing.conn.close_reason().is_none() {
                info!("gateway({gateway_id}) reconnected, superseding prior session");
                CloseReason::Reconnecting.close(&existing.conn);
            }
            gateways.remove(&gateway_id);
        }

        if gateways.len() >= MAX_GATEWAYS {
            warn!("gateway({}) rejected: registry full", conn.remote_address());
            return Err(CloseReason::RegistryFull);
        }

        gateways.insert(gateway_id, RelayEntry::new(gateway_id, conn, *pubkey));
        Ok(LifetimeP::HelloAck { resolver_time: now })
    }

    /// Evict the gateway entry once its connection closes (ptr-guarded against
    /// a racing re-registration), mirroring [`Self::watch_relay`].
    pub fn watch_gateway(self: &Arc<Self>, gateway_id: RelayId, conn: Arc<Connection>) {
        let resolver = self.clone();
        tokio::spawn(async move {
            let _ = conn.closed().await;
            resolver.remove_gateway_if_same(gateway_id, &conn);
        });
    }

    fn remove_gateway_if_same(&self, gateway_id: RelayId, conn: &Arc<Connection>) {
        let mut gateways = self.gateways.write();
        let same = gateways
            .get(&gateway_id)
            .map(|e| Arc::ptr_eq(&e.conn, conn))
            .unwrap_or(false);
        if same {
            gateways.remove(&gateway_id);
        }
    }

    /// Snapshot of the gateway directory for serving `GetGateways`.
    pub fn snapshot_gateways(&self) -> Vec<RelayEntry> {
        self.gateways.read().values().cloned().collect()
    }

    /// Closes resolver — best-effort kicks every registered relay and gateway
    /// so they stop trying to send into a soon-to-be-dead endpoint.
    pub fn close(&self) {
        for r in self.relays.read().values() {
            r.conn.close(CloseReason::ShuttingDown.code(), b"ResolverShuttingDown");
        }
        for g in self.gateways.read().values() {
            g.conn.close(CloseReason::ShuttingDown.code(), b"ResolverShuttingDown");
        }
    }
}

/// Shared id-binding + Ed25519 signature + timestamp-skew check used by
/// every authenticated [`LifetimeP`] packet that carries `(relay_id,
/// pubkey, timestamp, sig)`. Centralising the three checks in one place
/// keeps the policy uniform across packet kinds and makes future audits
/// trivial — the verification logic exists exactly once.
///
/// `kind` is a short tag inserted into rejection log lines (e.g. `"hello"`,
/// `"heartbeat"`) so operators can tell at a glance which packet was
/// rejected without parsing the rest of the line.
///
/// `signing_input` is the per-packet-kind transcript built by the matching
/// `relay_*_signing_input` helper; passing it in (instead of reconstructing
/// inside) keeps domain separation a caller-side concern and avoids this
/// helper having to know about every packet kind.
fn verify_signed_packet(
    addr: std::net::SocketAddr, kind: &str, relay_id: &RelayId, pubkey: &[u8; 32],
    sig: &[u8; 64], signing_input: &[u8], timestamp: u128,
) -> Result<(), CloseReason> {
    // 1. id <-> pubkey binding
    let expected_id = NodeId::new(pubkey);
    if &expected_id != relay_id {
        warn!("relay({addr}) rejected: {kind} relay_id does not match pubkey");
        return Err(CloseReason::BadSignature);
    }

    // 2. signature verification
    let vk = VerifyingKey::from_bytes(pubkey).map_err(|_| {
        warn!("relay({addr}) rejected: {kind} malformed Ed25519 pubkey");
        CloseReason::BadSignature
    })?;
    let signature = Signature::from_bytes(sig);
    if vk.verify_strict(signing_input, &signature).is_err() {
        warn!("relay({addr}) rejected: invalid {kind} signature");
        return Err(CloseReason::BadSignature);
    }

    // 3. timestamp freshness (replay protection)
    let now = systime().as_millis();
    let skew = now.abs_diff(timestamp);
    if skew > HELLO_MAX_SKEW_MS {
        warn!("relay({addr}) rejected: stale {kind} timestamp ({skew}ms skew)");
        return Err(CloseReason::StaleTimestamp);
    }

    Ok(())
}
