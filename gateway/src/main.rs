#![forbid(unsafe_code)]

mod cli {
    common::daemon_cli!("gateway", "Promtuz push gateway");
}
mod config;
mod fcm;
mod gateway;
mod quic;
mod registry;
mod store;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser as _;
use common::node::enroll::first_cert_der;
use common::server::accept;
use common::server::daemon;
use common::server::resolver_link;

use crate::config::AppConfig;
use crate::gateway::Gateway;

/// Sized for a carrier NAT and for a relay that dials once per wake; the per-connection request
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
    // Peers verify the cert's PUSH_GATEWAY capability on connect; the gateway does not check its
    // own.
    let key = daemon::start::<AppConfig>(
        "gateway",
        cli::VERSION,
        &cfg.network,
        &cli.config,
        cfg.log.level.as_deref(),
    )
    .await?;

    let tunnel_listener = common::quic::tunnel_listener::NodeTunnel::bind(
        &cfg.network, common::quic::tunnel::FEATURE_CONTROL,
    ).await?;
    let seeds = cfg.resolver.as_ref().map(|r| r.seed.clone()).unwrap_or_default();
    let cert = common::graceful!(first_cert_der(&cfg.network.cert_path), "reading the node cert");
    let gateway = Arc::new(Gateway::new(cfg));
    let tunnel = tunnel_listener.map(|listener| {
        let gateway = gateway.clone();
        listener.spawn(
            move |connection| {
                let gateway = gateway.clone();
                async move { quic::handler::Handler::handle(connection, gateway).await }
            },
            |channel, _mode| async move { channel.close(); },
        )
    });
    if gateway.store.is_some() {
        let gateway = gateway.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60 * 60));
            loop {
                tick.tick().await;
                if let Some(store) = &gateway.store {
                    store.sweep().await;
                }
            }
        });
    }
    tokio::spawn({
        let gateway = gateway.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            tick.tick().await;
            loop {
                tick.tick().await;
                gateway.wakes.retain_recent();
                gateway.wakes.shrink_to_fit();
                if let Err(e) = gateway.registry.sweep() {
                    common::warn!("gateway: push registry cleanup failed: {e:#}");
                }
            }
        }
    });

    let (_, mut link) = resolver_link::spawn(
        gateway.endpoint.clone(),
        seeds,
        key,
        resolver_link::Hello::Gateway(cert.to_vec()),
    );
    let mut acceptor = tokio::spawn(accept::serve(gateway.endpoint.clone(), ACCEPT, {
        let gateway = gateway.clone();
        move |connection| quic::handler::Handler::handle(connection, gateway.clone())
    }));

    let result = tokio::select! {
        _ = &mut acceptor => Err(anyhow!("acceptor stopped")),
        _ = &mut link => Err(anyhow!("resolver link stopped")),
        _ = daemon::shutdown_signal() => Ok(()),
    };

    // Otherwise the link redials the closing endpoint.
    link.abort();
    daemon::stop(&gateway.endpoint, tunnel).await;
    result
}
