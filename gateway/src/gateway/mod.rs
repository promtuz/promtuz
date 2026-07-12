use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use common::info;
use common::quic::config::build_server_cfg;
use common::quic::config::setup_crypto_provider;
use common::quic::id::NodeKey;
use common::quic::p256::secret_from_key;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;
use quinn::ServerConfig;

use crate::config::AppConfig;

/// The push gateway node: a blind QUIC listener that (later cuts) registers
/// `P → token` and dispatches wake requests to APNs/FCM. For now it enrolls
/// and listens. The `P → token` registry hangs here next cut.
pub struct Gateway {
    pub endpoint: Arc<Endpoint>,
}

impl Gateway {
    fn get_server_cfg(cfg: &AppConfig) -> Result<ServerConfig> {
        setup_crypto_provider()?;
        use ProtoRole as PR;
        // Dialers: devices register over `client/1`, home relays wake over
        // `relay/1`. The gateway itself never dials anyone over QUIC (FCM is
        // HTTPS), so it needs no ALPN role of its own.
        build_server_cfg(&cfg.network.cert_path, &cfg.network.key_path, &[PR::Client, PR::Relay])
    }

    fn endpoint(cfg: &AppConfig) -> Endpoint {
        let server_config = graceful!(Self::get_server_cfg(cfg), "building the TLS server config");
        let endpoint = graceful!(
            Endpoint::server(server_config, cfg.network.bind_addr()),
            "starting the QUIC endpoint"
        );
        if let Ok(addr) = endpoint.local_addr() {
            info!("gateway listening at QUIC({:?})", addr);
        }
        endpoint
    }

    pub fn new(cfg: AppConfig) -> Self {
        // Log our IPK for operator sanity; not stored — nothing in the wake
        // path re-signs with it yet.
        if let Ok(secret) = secret_from_key(&cfg.network.key_path) {
            if let Ok(key) = NodeKey::new(secret.verifying_key()) {
                info!("initializing gateway with IPK({})", key.key());
            }
        }
        Self { endpoint: Arc::new(Self::endpoint(&cfg)) }
    }
}
