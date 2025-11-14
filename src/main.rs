#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Result;
use common::quic::config::build_server_cfg;
// use common::quic::config::load_root_ca;
use common::quic::config::setup_crypto_provider;
use common::quic::protorole::ProtoRole;
use quinn::Endpoint;
use tokio::join;

use crate::quic::acceptor::run_acceptor;
use crate::util::config::AppConfig;

mod quic;
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

    // let roots = load_root_ca(&cfg.network.root_ca_path)?;
    let endpoint = Arc::new(Endpoint::server(server_cfg, cfg.network.address)?);

    if let Ok(addr) = endpoint.local_addr() {
        println!("QUIC(RESOLVER): listening at {:?}", addr);
    }

    let acceptor_handle = tokio::spawn(run_acceptor(endpoint.clone()));

    _ = join!(acceptor_handle);

    Ok(())
}
