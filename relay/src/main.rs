use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use clap::Parser as _;
use common::server::accept;
use common::server::daemon;
use common::server::resolver_link;
use tokio_util::sync::CancellationToken;

use crate::cli::Command;
use crate::dht::bootstrap;
use crate::dht::sync;
use crate::quic::resolver_link::ResolverLinkHandle;
use crate::relay::Relay;
use crate::storage::db::Store;
use crate::util::config::AppConfig;

mod cli;
mod control;
mod dht;
mod quic;
mod relay;
mod storage;
mod stunturn;
mod tcpassist;
#[cfg(test)]
mod tcp_control_tests;
#[cfg(test)]
mod test_support;
mod turn;
mod util;

/// Per source, and loose on purpose: a whole carrier NAT shares one address and reconnects at
/// once after an outage. The live caps are what bound concurrency.
const ACCEPT: accept::Policy =
    accept::Policy { per_minute: 600, burst: 300, max_live: 8192, max_live_per_source: 1024 };

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let cfg: AppConfig = daemon::load(&cli.config);

    match cli.command {
        Some(Command::ClearDb) => return control::clear_db_client(&cfg.control_socket).await,
        Some(Command::Enroll) => return common::node::enroll::interactive(&cfg.network),
        None => {},
    }

    crate::util::dht_log::DHT_LOG.store(cfg.log.dht, std::sync::atomic::Ordering::Relaxed);
    let key = daemon::start::<AppConfig>(
        "relay",
        cli::VERSION,
        &cfg.network,
        &cli.config,
        cfg.log.level.as_deref(),
    )
    .await?;

    let cancel = CancellationToken::new();

    let control_sock = cfg.control_socket.clone();
    let bound = Relay::bind(&cfg, &key);
    let store = Arc::new(common::graceful!(Store::open("db"), "opening the fjall store"));
    let relay = Arc::new(Relay::new(cfg, &key, bound, store));

    let tunnel_features = common::quic::tunnel::FEATURE_CONTROL
        | if relay.cfg.assist.tcp_enabled { common::quic::tunnel::FEATURE_ASSIST } else { 0 };
    let tunnel = common::quic::tunnel_listener::NodeTunnel::bind(&relay.cfg.network, tunnel_features)
        .await?
        .map(|listener| {
            let control_relay = relay.clone();
            let control_cancel = cancel.clone();
            let assist = Arc::new(tcpassist::Assist::default());
            listener.spawn(
                move |connection| {
                    let relay = control_relay.clone();
                    let cancel = control_cancel.clone();
                    async move { quic::handler::Handler::handle(connection, relay, cancel).await }
                },
                move |channel, mode| {
                    let assist = assist.clone();
                    async move { assist.serve(channel, mode).await }
                },
            )
        });

    let mut acceptor = tokio::spawn(accept::serve(relay.endpoint.clone(), ACCEPT, {
        let relay = relay.clone();
        let cancel = cancel.clone();
        move |connection| quic::handler::Handler::handle(connection, relay.clone(), cancel.clone())
    }));

    tokio::spawn(control::serve(relay.store.clone(), control_sock, cancel.clone()));
    tokio::spawn(quic::handler::client::events::presence::maintain(relay.clone(), cancel.clone()));

    if let Some(assist) = relay.assist.lock().take() {
        tokio::spawn(stunturn::serve(assist, cancel.clone()));
    }

    if let Some(turn) = relay.turn.clone() {
        tokio::spawn(turn.serve(cancel.clone()));
    }

    let (session, mut link) = resolver_link::spawn(
        relay.endpoint.clone(),
        relay.cfg.resolver.seed.clone(),
        key,
        resolver_link::Hello::Relay,
    );
    let resolver_handle = ResolverLinkHandle(session);

    if let Some(dht) = relay.dht.clone()
        && dht.cfg.enabled {
            dht.attach_resolver(resolver_handle.clone());

            // Both startup calls wait for a registered session. Called earlier they fail, and the
            // gateway list would stay empty for a whole refresh period.
            tokio::spawn({
                let (dht, resolver) = (dht.clone(), resolver_handle.clone());
                async move {
                    resolver.ready().await;
                    crate::dht::push_wake::refresh_gateways(dht, resolver).await
                }
            });

            // Detached so a slow resolver cannot delay accepting; the sync scheduler retries a
            // failed bootstrap.
            let resolver_handle_for_bootstrap = resolver_handle.clone();
            let dht_for_bootstrap = dht.clone();
            tokio::spawn(async move {
                resolver_handle_for_bootstrap.ready().await;
                match bootstrap::bootstrap(dht_for_bootstrap, resolver_handle_for_bootstrap).await {
                    Ok(()) => crate::dht_log!("DHT bootstrap complete"),
                    Err(bootstrap::BootstrapError::EmptyRegistry) => {
                        crate::dht_log!("DHT bootstrap: resolver returned no peers (new network?)")
                    },
                    Err(e) => crate::dht_log!("DHT bootstrap failed: {e}"),
                }
            });

            let dht_for_sched = dht.clone();
            let cancel_for_sched = cancel.clone();
            tokio::spawn(async move {
                sync::run_scheduler(dht_for_sched, cancel_for_sched).await;
            });
        }

    let result = tokio::select! {
        _ = &mut acceptor => Err(anyhow!("acceptor stopped")),
        _ = &mut link => Err(anyhow!("resolver link stopped")),
        _ = daemon::shutdown_signal() => Ok(()),
    };

    // Cancel first so connection tasks stop reading and finish in-flight fjall writes before the
    // endpoint goes away.
    cancel.cancel();
    // Otherwise the link redials the closing endpoint.
    link.abort();

    // Close DHT peers before the endpoint so in-flight peer RPCs see a clean close reason.
    if let Some(dht) = relay.dht.clone() {
        dht.shutdown().await;
    }

    daemon::stop(&relay.endpoint, tunnel).await;
    result
}
