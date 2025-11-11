use anyhow::Result;
use quinn::Endpoint;

use crate::quic::config::build_server_cfg;
use crate::quic::config::setup_crypto_provider;
use crate::util::config::AppConfig;

pub struct QuicEndpoint {
    pub endpoint: Endpoint,
}

impl QuicEndpoint {
    pub fn new(cfg: &AppConfig) -> Result<Self> {
        setup_crypto_provider()?;
        let server_cfg = build_server_cfg(cfg)?;
        let endpoint = Endpoint::server(server_cfg, cfg.network.address)?;
        if let Ok(addr) = endpoint.local_addr() {
            println!("QUIC: listening at {:?}", addr);
        }
        Ok(Self { endpoint })
    }
}
