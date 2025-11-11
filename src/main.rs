use anyhow::Result;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use quinn::Endpoint;

use crate::quic::dialer::connect_to_any_seed;
use crate::util::config::AppConfig;

mod quic;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load();

    setup_crypto_provider()?;
    let server_cfg = build_server_cfg(
        &cfg.network.cert_path,
        &cfg.network.key_path,
        &["resolver/1", "node/1", "client/1"],
    )?;

    let roots = load_root_ca(&cfg.network.root_ca_path)?;

    let mut endpoint = Endpoint::server(server_cfg, cfg.network.address)?;

    let client_cfg = build_client_cfg("node/1", &roots)?;
    endpoint.set_default_client_config(client_cfg);

    let conn = connect_to_any_seed(&endpoint, &cfg.resolver.seed, "arch.local").await?;

    Ok(())
}
