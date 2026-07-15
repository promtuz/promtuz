#![forbid(unsafe_code)]

mod cli;
mod config;
mod fcm;
mod gateway;
mod quic;
mod registry;
mod resolver_link;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::quic::CloseReason;

use crate::config::AppConfig;
use crate::gateway::Gateway;
use crate::quic::acceptor::Acceptor;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::get();

    let cfg = AppConfig::load(&cli.config, true);
    common::server::log::init(cfg.log.level.as_deref());
    common::info!("pzgateway {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));

    // Hold until we have a valid cert for our key (writes a CSR + waits if not
    // enrolled). The CA must sign it with the PUSH_GATEWAY capability — the
    // gateway does not enforce that on itself; peers verify it on connect.
    let csr_path = cfg.network.key_path.with_extension("csr");
    common::node::enroll::ensure_enrolled(&cfg.network, &csr_path, "gateway").await?;
    if cfg.network.watch_reload {
        common::node::enroll::spawn_config_reload(cli.config.clone());
    }

    let gateway = Arc::new(Gateway::new(cfg));
    let acceptor = Acceptor::new(gateway.clone());

    tokio::select! {
        _ = acceptor.run() => {}
        _ = tokio::signal::ctrl_c() => {
            println!();
            gateway
                .endpoint
                .close(CloseReason::ShuttingDown.code(), b"ShuttingDown");
            let _ =
                tokio::time::timeout(Duration::from_secs(5), gateway.endpoint.wait_idle()).await;
            common::info!("CLOSING GATEWAY");
        }
    }

    Ok(())
}
