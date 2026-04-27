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
use common::quic::p256::secret_from_key_or_create;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use parking_lot::RwLock;
use quinn::ClientConfig;
use quinn::Connection;
use quinn::Endpoint;
use rust_rocksdb::DB as RocksDB;

use crate::util::config::AppConfig;
use crate::util::rocksdb::rocksdb;

/// Long-term Ed25519 *identity* keypair for this relay.
///
/// This is **not** the TLS server key. The TLS key (loaded directly by
/// `build_server_cfg` from `cfg.network.key_path`) lives only inside
/// rustls/aws-lc-rs and never touches the application layer. The identity
/// key here is what signs application-layer messages on this relay's
/// behalf — currently `RelayHello` to the resolver — and is what derives
/// the public-facing `relay_id`. Splitting the two trust roots means a
/// TLS-layer key disclosure does not turn into a permanent identity
/// compromise.
#[derive(Debug)]
pub struct RelayKeys {
    pub signing: SigningKey,
    pub public:  VerifyingKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self, ()> {
        // Loaded from `identity_key_path`, distinct from `key_path` (which
        // is the TLS server key). The file is auto-created on first run.
        let secret = secret_from_key_or_create(&cfg.network.identity_key_path)?;
        let public = secret.verifying_key();

        Ok(Self { signing: secret, public })
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

    /// Long-lived Ed25519 identity keypair. Held here so things that need to
    /// sign on this relay's behalf (e.g. `RelayHello` to the resolver) don't
    /// have to re-read the on-disk PKCS#8 file every time.
    pub keys: RelayKeys,
    /// SystemTime in ms since EPOCH when relay is started first
    // pub start_ms: u128,
    pub endpoint: Endpoint,

    pub cfg: AppConfig,

    pub client_cfg: Arc<ClientConfig>,
    pub peer_client_cfg: Arc<ClientConfig>,

    pub rocks: RocksDB,

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

        let rocks = graceful!(rocksdb(), "failed to setup rocksdb");
        let clients = RwLock::new(HashMap::new());

        Self { key, keys, cfg, client_cfg, peer_client_cfg, rocks, endpoint, clients }
    }
}
