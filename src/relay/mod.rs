use std::fs;
use std::sync::Arc;

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
use tokio::sync::Mutex;

use crate::util::config::AppConfig;
use crate::util::systime;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub secret: p256::SecretKey,
    pub public: p256::PublicKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self> {
        let sec = fs::read_to_string(&cfg.network.key_path)?;

        let secret = if sec.starts_with("-----BEGIN EC PRIVATE KEY-----") {
            SecretKey::from_sec1_pem(&sec)?
        } else {
            SecretKey::from_pkcs8_pem(&sec)?
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

    pub keys: RelayKeys,

    /// SystemTime in ms since EPOCH when relay is started first
    pub start_ms: u128,

    pub endpoint: Arc<Endpoint>,

    pub cfg: AppConfig,

    pub client_cfg: Arc<ClientConfig>,
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
        let keys = graceful!(RelayKeys::from_cfg(&cfg), "RELAY_ERR:");
        let id = derive_node_id(&keys.public);

        println!("RELAY: Initializing with ID({})", id);

        let mut endpoint = Self::endpoint(&cfg);

        let roots = graceful!(load_root_ca(&cfg.network.root_ca_path), "CA_ERR:");

        let client_cfg =
            Arc::new(graceful!(build_client_cfg(ProtoRole::Relay, &roots), "CLIENT_CFG_ERR:"));

        endpoint.set_default_client_config((*client_cfg).clone());

        Self {
            id,
            keys,
            start_ms: systime().as_millis(),
            endpoint: Arc::new(endpoint),
            cfg,
            client_cfg,
        }
    }
}
