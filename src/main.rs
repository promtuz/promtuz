use std::sync::Arc;

use anyhow::Result;
use common::quic::config::build_client_cfg;
use common::quic::config::build_server_cfg;
use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;

use crate::relay::Relay;
use crate::util::config::AppConfig;

mod quic;
mod relay;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

    use ProtoRole as PR;

    setup_crypto_provider()?;
    let server_cfg = {
        build_server_cfg(
            &cfg.network.cert_path,
            &cfg.network.key_path,
            &[PR::Resolver, PR::Relay, PR::Peer, PR::Client],
        )?
    };

    let roots = load_root_ca(&cfg.network.root_ca_path)?;
    let mut endpoint = Endpoint::server(server_cfg, cfg.network.address)?;

    if let Ok(addr) = endpoint.local_addr() {
        println!("QUIC(RELAY): listening at {:?}", addr);
    }

    let client_cfg = build_client_cfg(PR::Relay, &roots)?;
    endpoint.set_default_client_config(client_cfg);

    let relay = Arc::new(Relay::init(&cfg, endpoint).await?);

    relay.hello().await?;

    let resolver_handle = tokio::spawn({
        let relay = relay.clone();
        async move { relay.handle_resolver().await }
    });

    tokio::select! {
        _ = resolver_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            relay.close();
            relay.endpoint.wait_idle().await;
            
            println!("CLOSING RELAY");
        }
    }

    Ok(())
}
