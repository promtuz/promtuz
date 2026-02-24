use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::graceful;
use common::info;
use common::node::config::NodeSeed;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeId;
use common::quic::id::derive_node_id;
use common::quic::p256::PublicKey;
use common::quic::p256::SecretKey;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use quinn::ClientConfig;
use quinn::Endpoint;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

use crate::dht::Dht;
use crate::dht::DhtParams;
use crate::dht::NodeContact;
use crate::quic::handler::peer::{send_dht_request};
use crate::quic::msg::dht::{DhtRequest, DhtResponse};
use crate::util::config::AppConfig;
use crate::util::systime;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub secret: SecretKey,
    pub public: PublicKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self, ()> {
        let secret = secret_from_key(&cfg.network.key_path)?;

        Ok(Self { public: secret.public_key(), secret })
    }
}

pub type RelayRef = Arc<Mutex<Relay>>;

/// Represents a single relay node running in the network.
///
/// It's *local identity* of the relay process,
/// not a message exchanged over the wire.
///
/// It's apparently like a core process handler
#[derive(Debug)]
pub struct Relay {
    pub id: NodeId,

    // pub keys: RelayKeys,
    /// SystemTime in ms since EPOCH when relay is started first
    // pub start_ms: u128,
    pub endpoint: Arc<Endpoint>,

    pub cfg: AppConfig,

    pub client_cfg: Arc<ClientConfig>,
    pub peer_client_cfg: Arc<ClientConfig>,

    /// Shared in-memory DHT state
    pub dht: Arc<RwLock<Dht>>,
}

impl Relay {
    fn endpoint(cfg: &AppConfig) -> Endpoint {
        use ProtoRole as PR;

        graceful!(setup_crypto_provider(), "CRYPTO_ERR:");

        let server_cfg = graceful!(
            build_server_cfg(
                &cfg.network.cert_path,
                &cfg.network.key_path,
                &[PR::Resolver, PR::Relay, PR::Peer, PR::Client],
            ),
            "SERVER_CFG_ERR:"
        );

        let endpoint = graceful!(Endpoint::server(server_cfg, cfg.network.address), "QUIC_ERR:");
        if let Ok(addr) = endpoint.local_addr() {
            info!("relay listening at QUIC({:?})", addr);
        }
        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let keys = RelayKeys::from_cfg(&cfg).expect("config failed");
        let id = derive_node_id(&keys.public);

        info!("initializing Relay with ID({})", id);

        let mut endpoint = Self::endpoint(&cfg);

        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "CA_ERR:");

        let client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Relay, &roots), "CLIENT_CFG_ERR:"));
        let peer_client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Peer, &roots), "PEER_CFG_ERR:"));

        endpoint.set_default_client_config((*client_cfg).clone());

        let params = DhtParams {
            bucket_size: cfg.dht.bucket_size,
            k: cfg.dht.k,
            alpha: cfg.dht.alpha,
            user_ttl: Duration::from_secs(cfg.dht.user_ttl_secs),
            republish_interval: Duration::from_secs(cfg.dht.republish_secs),
        };

        let dht = Arc::new(RwLock::new(Dht::new(id, Some(params))));

        Self {
            id,
            // keys,
            // start_ms: systime().as_millis(),
            endpoint: Arc::new(endpoint),
            cfg,
            client_cfg,
            peer_client_cfg,
            dht,
        }
    }

    /// Spawn background maintenance for the DHT: cleanup & refresh.
    pub fn spawn_dht_tasks(relay: RelayRef) {
        // periodic cleanup of expired user records
        tokio::spawn({
            let relay = relay.clone();
            async move {
                loop {
                    let republish_interval = {
                        relay.lock().await.cfg.dht.republish_secs
                    };
                    {
                        let dht = { relay.lock().await.dht.clone() };
                        let mut dht = dht.write().await;
                        dht.cleanup_expired();
                    }
                    tokio::time::sleep(Duration::from_secs(republish_interval)).await;
                }
            }
        });

        // Bootstrap: ping seeds then do a self-lookup to populate routing table
        tokio::spawn(async move {
            let seeds = { relay.lock().await.cfg.dht.bootstrap.clone() };
            for seed in seeds {
                if let Err(err) = Self::bootstrap_peer(relay.clone(), seed.clone()).await {
                    common::error!("DHT: bootstrap peer {} failed: {}", seed.addr, err);
                }
            }
            // After pinging seeds, look up our own ID to discover nearby nodes
            if let Err(err) = Self::self_lookup(relay.clone()).await {
                common::error!("DHT: self-lookup failed: {}", err);
            }
        });
    }

    /// Ping a seed peer and add it to the routing table if it responds.
    async fn bootstrap_peer(relay: RelayRef, seed: NodeSeed) -> Result<()> {
        let contact = NodeContact {
            id: seed.id,
            addr: seed.addr,
            last_seen: systime().as_secs(),
        };

        let (local_id, local_addr) = {
            let r = relay.lock().await;
            let addr = r.endpoint.local_addr().unwrap_or(r.cfg.network.address);
            (r.id, addr)
        };

        let req = DhtRequest::Ping { from: local_id, addr: local_addr };
        let resp = send_dht_request(relay.clone(), contact.clone(), req).await?;

        match resp {
            DhtResponse::Pong { from } => {
                info!("DHT: bootstrap pong from {} ({})", from, seed.addr);
                let dht = { relay.lock().await.dht.clone() };
                let mut dht = dht.write().await;
                dht.upsert_node(NodeContact {
                    id: from,
                    addr: seed.addr,
                    last_seen: systime().as_secs(),
                });
            },
            other => {
                anyhow::bail!("unexpected response from seed: {:?}", other);
            },
        }
        Ok(())
    }

    /// Kademlia self-lookup: ask known nodes for nodes closest to our own ID.
    /// This populates our routing table beyond just the configured seeds.
    async fn self_lookup(relay: RelayRef) -> Result<()> {
        let (local_id, k) = {
            let r = relay.lock().await;
            let k = r.cfg.dht.k;
            (r.id, k)
        };

        let targets = {
            let dht = { relay.lock().await.dht.clone() };
            let dht = dht.read().await;
            dht.get_closest_nodes(local_id, k)
        };

        for target in targets {
            let req = DhtRequest::FindNode { target: local_id };
            match send_dht_request(relay.clone(), target.clone(), req).await {
                Ok(DhtResponse::NodeResult { nodes }) => {
                    let dht = { relay.lock().await.dht.clone() };
                    let mut dht = dht.write().await;
                    for node in nodes {
                        dht.upsert_node(node);
                    }
                },
                Ok(other) => {
                    info!("DHT: self-lookup to {} unexpected: {:?}", target.id, other);
                },
                Err(err) => {
                    info!("DHT: self-lookup to {} failed: {}", target.id, err);
                },
            }
        }
        Ok(())
    }
}
