use anyhow::Result;
use common::quic::config::build_server_cfg;
use common::quic::config::setup_crypto_provider;
use quinn::Endpoint;

use crate::util::config::AppConfig;

pub struct QuicEndpoint {
    pub endpoint: Endpoint,
}

impl QuicEndpoint {
    pub fn new(cfg: &AppConfig) -> Result<Self> {
        setup_crypto_provider()?;
        let server_cfg = build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &["resolver/1", "node/1", "client/1"],
        )?;
        let endpoint = Endpoint::server(server_cfg, cfg.network.address)?;
        if let Ok(addr) = endpoint.local_addr() {
            println!("QUIC: listening at {:?}", addr);
        }
        Ok(Self { endpoint })
    }
}
