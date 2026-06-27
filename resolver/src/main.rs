#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

// mod proto;
mod cli;
mod quic;
mod resolver;
mod util;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::quic::CloseReason;

use crate::quic::acceptor::Acceptor;
use crate::resolver::Resolver;
use crate::util::config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::get();

    let cfg = AppConfig::load(&cli.config, true);
    common::server::log::init(cfg.log.level.as_deref());
    common::info!("pzresolver {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));

    // Hold here until we have a valid cert for our key (writes a CSR + waits
    // if not enrolled), so the endpoint is only built with usable TLS material.
    let csr_path = cli.config.with_extension("csr");
    common::node::enroll::ensure_enrolled(&cfg.network, &csr_path, "resolver").await?;
    if cfg.network.watch_reload {
        common::node::enroll::spawn_config_reload(cli.config.clone());
    }

    let resolver = Arc::new(Resolver::new(cfg));
    let acceptor = Acceptor::new(resolver.endpoint.clone());

    let acceptor_handle = tokio::spawn({
        let resolver = resolver.clone();
        async move { acceptor.run(resolver.clone()).await }
    });

    tokio::select! {
        _ = acceptor_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            // Kick registered relays *before* tearing down the endpoint so
            // they observe a clean close reason rather than a transport
            // timeout.
            resolver.close();
            resolver
                .endpoint
                .close(CloseReason::ShuttingDown.code(), b"ShuttingDown");

            // Give in-flight closes a brief window to flush before exit.
            // Bounded so a misbehaving peer can't stall shutdown forever.
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                resolver.endpoint.wait_idle(),
            )
            .await;

            common::info!("CLOSING RESOLVER");
        }
    }

    Ok(())
}
