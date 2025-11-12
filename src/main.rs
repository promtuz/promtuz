use anyhow::Result;
use common::PROTOCOL_VERSION;
use common::msg::resolver::NodeHello;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;

use crate::quic::dialer::connect_to_any_seed;
use crate::util::config::AppConfig;

mod quic;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load();

    use ProtoRole as PR;

    setup_crypto_provider()?;
    let server_cfg = {
        build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &[PR::Resolver, PR::Node, PR::Peer, PR::Client],
        )?
    };

    let roots = load_root_ca(&cfg.network.root_ca_path)?;

    let mut endpoint = Endpoint::server(server_cfg, cfg.network.address)?;

    // START ACcEPTOR

    let client_cfg = build_client_cfg(PR::Node, &roots)?;
    endpoint.set_default_client_config(client_cfg);

    let conn = connect_to_any_seed(&endpoint, &cfg.resolver.seed, "arch.local").await?;

    let mut stream = conn.open_bi().await?;

    let hello = NodeHello {
        node_id: node.identity_id.clone(),
        address: node.public_addr.clone(),
        version: PROTOCOL_VERSION
    };

    Ok(())
}
