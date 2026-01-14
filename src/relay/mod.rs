use std::fs;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::graceful;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeId;
use common::quic::id::derive_node_id;
use common::quic::protorole::ProtoRole;
use p256::SecretKey;
use p256::pkcs8::DecodePrivateKey;
use quinn::ClientConfig;
use quinn::Endpoint;
use tokio::sync::{Mutex, RwLock};

use crate::dht::{Dht, DhtParams, NodeContact};
use crate::util::config::AppConfig;
use crate::util::config::PeerSeed;
use crate::util::systime;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub secret: p256::SecretKey,
    pub public: p256::PublicKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self, ()> {
        let sec = fs::read_to_string(&cfg.network.key_path).map_err(|err| {
            eprintln!("ERROR: failed to read file {path:?}: {err}", path = &cfg.network.key_path);
        })?;
        
        let secret = if sec.starts_with("-----BEGIN EC PRIVATE KEY-----") {
            SecretKey::from_sec1_pem(&sec).map_err(|err| { 
                eprintln!("ERROR: failed to parse sec1 secret key: {err}");
            })?
        } else {
            SecretKey::from_pkcs8_pem(&sec).map_err(|err| {
                eprintln!("ERROR: failed to parse pkcs8 secret key: {err}");
            })?
        };

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
            println!("QUIC(RELAY): listening at {:?}", addr);
        }
        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        let keys = RelayKeys::from_cfg(&cfg).expect("config failed");
        let id = derive_node_id(&keys.public);

        println!("RELAY: Initializing with ID({})", id);

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
        // periodic cleanup and republish placeholder
        tokio::spawn({
            let relay = relay.clone();
            async move {
                loop {
                    let (republish_interval, user_ttl) = {
                        let r = relay.lock().await;
                        (r.cfg.dht.republish_secs, r.cfg.dht.user_ttl_secs)
                    };
                    {
                        let dht = { relay.lock().await.dht.clone() };
                        let mut dht = dht.write().await;
                        dht.cleanup_expired();
                        // future: republish of active users can hook here
                        let _ = user_ttl;
                    }
                    tokio::time::sleep(Duration::from_secs(republish_interval)).await;
                }
            }
        });

        // Bootstrap routing table with configured peers
        tokio::spawn(async move {
            let seeds = { relay.lock().await.cfg.dht.bootstrap.clone() };
            for seed in seeds {
                let _ = Self::bootstrap_peer(relay.clone(), seed).await;
            }
        });
    }

    async fn bootstrap_peer(relay: RelayRef, seed: PeerSeed) -> Result<()> {
        {
            let dht = { relay.lock().await.dht.clone() };
            let mut dht = dht.write().await;
            dht.upsert_node(NodeContact {
                id: seed.id,
                addr: seed.address,
                last_seen: systime().as_secs(),
            });
        }
        // Outbound ping can be added here once peer RPC client is wired.
        Ok(())
    }
}
