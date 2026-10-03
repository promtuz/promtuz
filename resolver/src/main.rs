#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

mod cli {
    common::daemon_cli!("resolver", "Promtuz resolver");
}
mod quic;
mod resolver;
mod util;

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser as _;
use common::quic::protorole::ProtoRole;
use common::server::accept;
use common::server::daemon;

use crate::resolver::Resolver;
use crate::util::config::AppConfig;

/// Sized for a carrier NAT: phones open a fresh connection per lookup, and the per-connection RPC
/// budget bounds what each connection costs.
const ACCEPT: accept::Policy =
    accept::Policy { per_minute: 600, burst: 300, max_live: 4096, max_live_per_source: 256 };

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let cfg: AppConfig = daemon::load(&cli.config);
    if let Some(cli::Command::Enroll) = cli.command {
        return common::node::enroll::interactive(&cfg.network);
    }
    daemon::start::<AppConfig>(
        "resolver",
        cli::VERSION,
        &cfg.network,
        &cli.config,
        cfg.log.level.as_deref(),
    )
    .await?;

    let roles = &[ProtoRole::Resolver, ProtoRole::Relay, ProtoRole::Client];
    let endpoint = daemon::bind(&cfg.network, roles, "resolver");
    let resolver = Arc::new(Resolver::new(cfg, endpoint));
    let tunnel = common::quic::tunnel_listener::NodeTunnel::bind(
        &resolver.cfg.network, common::quic::tunnel::FEATURE_CONTROL,
    ).await?.map(|listener| {
        let resolver = resolver.clone();
        listener.spawn(
            move |connection| {
                let resolver = resolver.clone();
                async move { quic::handler::Handler::handle(connection, resolver).await }
            },
            |channel, _mode| async move { channel.close(); },
        )
    });

    let mut acceptor = tokio::spawn(accept::serve(resolver.endpoint.clone(), ACCEPT, {
        let resolver = resolver.clone();
        move |connection| quic::handler::Handler::handle(connection, resolver.clone())
    }));

    let result = tokio::select! {
        _ = &mut acceptor => Err(anyhow!("acceptor stopped")),
        _ = daemon::shutdown_signal() => Ok(()),
    };

    // Kick registered relays and gateways before closing the endpoint, so they see a clean close
    // reason rather than a transport timeout.
    resolver.close();
    daemon::stop(&resolver.endpoint, tunnel).await;
    result
}
