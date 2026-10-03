//! Relay-to-relay mesh: Kademlia routing over `peer/5`, the sticky-home offline queue and the MLS
//! stash relay.


pub(crate) mod bootstrap;
pub mod config;
pub(crate) mod forward;
pub(crate) mod handler;
pub(crate) mod home;
pub(crate) mod lookup;
pub(crate) mod mls;
pub(crate) mod peer_dial;
pub(crate) mod push_replication;
pub(crate) mod push_wake;
pub(crate) mod queue_drain;
pub(crate) mod rate_limit;
pub(crate) mod routing;
pub(crate) mod rpc;
pub(crate) mod store;
pub(crate) mod sync;
pub(crate) mod tls_extract;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;

use common::proto::RelayId;
use common::proto::client_res::GatewayDescriptor;
use common::quic::id::NodeId;
pub use config::DhtConfig;
use ed25519_dalek::SigningKey;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;

use self::mls::kp::KpFetchLimiters;
use self::mls::welcome::WelcomeLimiters;
use self::routing::RoutingTable;
use crate::quic::resolver_link::ResolverLinkHandle;
use crate::storage::db::Store;

/// DHT runtime state. Every lock here is `parking_lot` and is never held across an `await`.
#[derive(Debug)]
pub struct Dht {
    pub(crate) routing: RwLock<RoutingTable>,

    pub(crate) store: Arc<Store>,

    /// Live `peer/5` connections with each peer's verified key. Outbound: the cert SPKI, whose
    /// `BLAKE3(SPKI) == NodeId` pin after the handshake is the dial's only identity check.
    /// Inbound: the verified `DhtHello` pubkey.
    pub(crate) peer_conns: RwLock<HashMap<NodeId, (Connection, [u8; 32])>>,

    pub(crate) resolver: parking_lot::RwLock<Option<ResolverLinkHandle>>,

    pub node_id: NodeId,

    pub signing_key: SigningKey,

    pub cfg: DhtConfig,

    pub(crate) endpoint: Option<Endpoint>,

    pub(crate) peer_client_cfg: Option<Arc<ClientConfig>>,

    pub(crate) rate_limiters: rate_limit::PerPeerLimiters,

    pub(crate) kp_fetch_limiters: KpFetchLimiters,

    pub(crate) welcome_limiters: WelcomeLimiters,

    pub(crate) wake_limiter: rate_limit::KeyedLimiter<[u8; 32]>,

    #[cfg(test)]
    pub(crate) limiter_clock: rate_limit::LimiterClock,

    /// One pending-push replay at a time, rotating through the backlog.
    pub(crate) push_retry_in_flight: AtomicBool,
    pub(crate) push_retry_cursor: AtomicUsize,

    /// The relay's authenticated clients, so a home can deliver straight to a recipient
    /// connected here.
    pub(crate) clients: Option<ClientsMap>,

    /// Current user-signed active-relay leases. Shared with the client
    /// handler so a lease relay can reject stale cross-relay routes.
    pub(crate) presence_leases: Option<PresenceLeases>,

    /// Gateways from the resolver whose certificate carries `PUSH_GATEWAY`. Empty means no wakes.
    pub(crate) push_gateways: PushGateways,

    /// One live connection per gateway, redialed once it has closed.
    pub(crate) gateway_conns: RwLock<HashMap<RelayId, Connection>>,
}

pub(crate) type ClientsMap = Arc<RwLock<HashMap<[u8; 32], Connection>>>;
pub(crate) type PresenceLeases = Arc<RwLock<HashMap<[u8; 32], common::proto::dht_p2p::PresenceLease>>>;

pub(crate) type PushGateways = Arc<RwLock<Vec<GatewayDescriptor>>>;

impl Dht {
    pub fn new(
        node_id: NodeId, signing_key: SigningKey, cfg: DhtConfig, store: Arc<Store>,
    ) -> Self {
        let clock = rate_limit::LimiterClock::default();
        Self {
            routing: RwLock::new(RoutingTable::empty(node_id)),
            store,
            peer_conns: RwLock::new(HashMap::new()),
            resolver: parking_lot::RwLock::new(None),
            node_id,
            signing_key,
            cfg,
            endpoint: None,
            peer_client_cfg: None,
            rate_limiters: rate_limit::PerPeerLimiters::new(&clock),
            kp_fetch_limiters: KpFetchLimiters::new(&clock),
            welcome_limiters: WelcomeLimiters::new(&clock),
            wake_limiter: push_wake::wake_limiter(&clock),
            #[cfg(test)]
            limiter_clock: clock,
            push_retry_in_flight: AtomicBool::new(false),
            push_retry_cursor: AtomicUsize::new(0),
            clients: None,
            presence_leases: None,
            push_gateways: Arc::new(RwLock::new(Vec::new())),
            gateway_conns: RwLock::new(HashMap::new()),
        }
    }

    /// Drop limiter rows whose buckets have refilled. Keys are free to mint,
    /// so without this each keyed limiter is a slow leak.
    pub(crate) fn sweep_limiters(&self) {
        self.rate_limiters.sweep();
        self.kp_fetch_limiters.sweep();
        self.welcome_limiters.sweep();
        self.wake_limiter.retain_recent();
        self.wake_limiter.shrink_to_fit();
    }

    pub fn attach_dialer(&mut self, endpoint: Endpoint, peer_client_cfg: Arc<ClientConfig>) {
        self.endpoint = Some(endpoint);
        self.peer_client_cfg = Some(peer_client_cfg);
    }

    pub fn attach_clients(&mut self, clients: Arc<RwLock<HashMap<[u8; 32], Connection>>>) {
        self.clients = Some(clients);
    }

    pub fn attach_presence_leases(&mut self, leases: PresenceLeases) {
        self.presence_leases = Some(leases);
    }

    pub fn attach_resolver(&self, handle: ResolverLinkHandle) {
        *self.resolver.write() = Some(handle);
    }

    pub async fn shutdown(&self) {
        use common::quic::CloseReason;
        let conns: Vec<Connection> = {
            let mut guard = self.peer_conns.write();
            guard.drain().map(|(_, (c, _pk))| c).collect()
        };
        for conn in conns {
            CloseReason::ShuttingDown.close(&conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use common::proto::mls_wire::MLS_WIRE_VERSION;
    use common::proto::mls_wire::welcome_ack_signing_input;
    use common::proto::mls_wire::welcome_fetch_signing_input;
    use common::utils::now_ms;
    use ed25519_dalek::Signer;

    use super::forward::forward_to_homes;
    use super::mls::kp_originate::originate_fetch;
    use super::mls::kp_originate::originate_publish;
    use super::mls::welcome_originate::originate_welcome_ack;
    use super::mls::welcome_originate::originate_welcome_fetch;
    use super::mls::welcome_originate::originate_welcome_publish;
    use super::rate_limit::RpcClass;
    use super::*;
    use crate::test_support::dht;
    use crate::test_support::dispatch;
    use crate::test_support::ipk;
    use crate::test_support::key;
    use crate::test_support::kp_record;
    use crate::test_support::kp_sig;
    use crate::test_support::queued;
    use crate::test_support::unreachable_peer;
    use crate::test_support::welcome;

    const HOUR: u64 = 3_600_000;

    /// One relay is a whole network: its only selected home is a write quorum for messages,
    /// KeyPackages and Welcomes. Once a second home is selected, its failure never shrinks the
    /// quorum to one, and the message stays in local custody. Rows: (second home, quorum met).
    #[tokio::test]
    async fn a_lone_home_is_a_quorum_and_a_failed_second_home_is_not() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        for (second_home, quorum) in [(false, true), (true, false)] {
            let seed = if second_home { 20 } else { 10 };
            if second_home {
                dht.routing.write().insert(unreachable_peer(2));
            }
            let now = now_ms();
            let to = ipk(seed);
            let sent = dispatch(&key(seed + 1), to, [seed; 16], b"offline");
            assert_eq!(forward_to_homes(dht.clone(), sent, now).await.is_ok(), quorum);
            assert_eq!(queued(&dht, &to), [[seed; 16]]);

            let owner = key(seed + 2);
            let records = vec![kp_record(&owner, [seed; 32], now + HOUR)];
            let sig = kp_sig(&owner, &records, now);
            let published = originate_publish(&dht, ipk(seed + 2), records, now, sig).await;
            assert_eq!((published.homes_succeeded, published.quorum_met), (1, quorum));

            let invite = welcome(&key(seed + 3), ipk(seed + 4), seed);
            assert_eq!(originate_welcome_publish(&dht, invite, now).await, quorum);
        }
    }

    /// A lone relay stores, serves and retires KeyPackages and Welcomes by itself, and an
    /// unsigned publication stores nothing.
    #[tokio::test]
    async fn a_lone_relay_serves_and_retires_what_it_stored() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        let (owner, now) = (key(3), now_ms());
        let owner_ipk = owner.verifying_key().to_bytes();
        let packages =
            vec![kp_record(&owner, [1; 32], now + HOUR), kp_record(&owner, [2; 32], now + HOUR)];
        let unsigned = originate_publish(&dht, owner_ipk, packages.clone(), now, [0; 64]).await;
        assert_eq!((unsigned.homes_succeeded, unsigned.quorum_met), (0, false));
        assert!(originate_fetch(&dht, owner_ipk, now).await.record.is_none());
        let sig = kp_sig(&owner, &packages, now);
        assert!(originate_publish(&dht, owner_ipk, packages, now, sig).await.quorum_met);
        let mut vended = Vec::new();
        for _ in 0..2 {
            vended.push(originate_fetch(&dht, owner_ipk, now).await.record.unwrap().kp_ref.0[0]);
        }
        vended.sort();
        assert_eq!(vended, [1, 2], "each package is vended once");
        assert!(originate_fetch(&dht, owner_ipk, now).await.record.is_none());

        let recipient = key(4);
        let recipient_ipk = recipient.verifying_key().to_bytes();
        let mut forged = welcome(&key(5), recipient_ipk, 1);
        forged.sender_sig.0[0] ^= 1;
        assert!(!originate_welcome_publish(&dht, forged, now).await);
        assert!(originate_welcome_publish(&dht, welcome(&key(5), recipient_ipk, 1), now).await);
        let fetch_sig = || {
            let msg =
                welcome_fetch_signing_input(MLS_WIRE_VERSION, &recipient_ipk, &dht.node_id, now);
            recipient.sign(&msg).to_bytes()
        };
        let entries = originate_welcome_fetch(&dht, recipient_ipk, now, fetch_sig()).await;
        let ids: Vec<[u8; 8]> = entries.iter().map(|entry| entry.welcome_id.0).collect();
        assert_eq!(ids.len(), 1);
        let msg =
            welcome_ack_signing_input(MLS_WIRE_VERSION, &recipient_ipk, &dht.node_id, &ids, now);
        originate_welcome_ack(&dht, recipient_ipk, ids, now, recipient.sign(&msg).to_bytes()).await;
        assert!(originate_welcome_fetch(&dht, recipient_ipk, now, fetch_sig()).await.is_empty());
    }

    /// RLY-05: keys are free to mint, so every keyed limiter must forget a key once its bucket
    /// has refilled, and keep it until then.
    #[test]
    fn sweeping_forgets_limiter_keys_once_their_buckets_refill() {
        let (_dir, dht) = dht(NodeId::from_bytes([1; 32]));
        let peer = NodeId::from_bytes([2; 32]);
        for class in [RpcClass::Cheap, RpcClass::Expensive, RpcClass::Bulk] {
            dht.rate_limiters.check(&peer, class).unwrap();
        }
        dht.kp_fetch_limiters.check(&[3; 32], &peer).unwrap();
        dht.welcome_limiters.check(&peer).unwrap();
        dht.wake_limiter.check_key(&[4; 32]).unwrap();
        let keys = |dht: &Dht| {
            let peers = &dht.rate_limiters;
            [
                peers.cheap.len(),
                peers.expensive.len(),
                peers.bulk.len(),
                dht.kp_fetch_limiters.per_pair.len(),
                dht.kp_fetch_limiters.per_target.len(),
                dht.welcome_limiters.limiter.len(),
                dht.wake_limiter.len(),
            ]
        };
        dht.sweep_limiters();
        assert_eq!(keys(&dht), [1; 7]);
        dht.limiter_clock.advance(Duration::from_secs(3600));
        dht.sweep_limiters();
        assert_eq!(keys(&dht), [0; 7]);
    }
}
