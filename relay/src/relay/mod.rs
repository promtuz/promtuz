use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeKey;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::VerifyingKey;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;


use crate::util::config::AppConfig;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub public: VerifyingKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self, ()> {
        let secret = secret_from_key(&cfg.network.key_path)?;

        Ok(Self { public: secret.verifying_key() })
    }
}

pub type RelayRef = Arc<Relay>;

/// Represents a single relay node running in the network.
///
/// It's *local identity* of the relay process,
/// not a message exchanged over the wire.
///
/// It's apparently like a core process handler
#[derive(Debug)]
pub struct Relay {
    pub key: NodeKey,

    // pub keys: RelayKeys,
    /// SystemTime in ms since EPOCH when relay is started first
    // pub start_ms: u128,
    pub endpoint: Arc<Endpoint>,

    pub cfg: AppConfig,

    pub client_cfg: Arc<ClientConfig>,
    pub peer_client_cfg: Arc<ClientConfig>,

    /// Shared in-memory DHT state
    // pub dht: Arc<RwLock<Dht>>,

    /// Connected + authenticated clients, keyed by IPK
    pub clients: RwLock<HashMap<[u8; 32], Connection>>,
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
        let key = NodeKey::new(keys.public).expect("invalid public key length");

        info!("initializing Relay with ID({key})");

        let mut endpoint = Self::endpoint(&cfg);

        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "CA_ERR:");

        let client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Relay, &roots), "CLIENT_CFG_ERR:"));
        let peer_client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Peer, &roots), "PEER_CFG_ERR:"));

        endpoint.set_default_client_config((*client_cfg).clone());

        // let params = DhtParams {
        //     bucket_size: cfg.dht.bucket_size,
        //     k: cfg.dht.k,
        //     alpha: cfg.dht.alpha,
        //     user_ttl: Duration::from_secs(cfg.dht.user_ttl_secs),
        //     republish_interval: Duration::from_secs(cfg.dht.republish_secs),
        // };

        // let dht = Arc::new(RwLock::new(Dht::new(key, Some(params))));

        Self {
            key,
            cfg,
            client_cfg,
            peer_client_cfg,
            // dht,
            endpoint: Arc::new(endpoint),
            clients: RwLock::new(HashMap::new()),
        }
    }
}
