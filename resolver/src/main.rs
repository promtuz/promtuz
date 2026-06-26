#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

// mod proto;
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
    // `--version` / `-V`: report and exit before touching config or the runtime.
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("pzresolver {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));
        return Ok(());
    }

    let cfg = AppConfig::load(true);
    common::server::log::init(cfg.log.level.as_deref());
    common::info!("pzresolver {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));

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
