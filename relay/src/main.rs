use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::info;
use common::quic::CloseReason;
use tokio_util::sync::CancellationToken;

use crate::dht::bootstrap;
use crate::dht::sync;
use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::util::config::AppConfig;

mod cli;
mod cmd;
mod control;
mod dht;
mod quic;
mod relay;
mod storage;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::get();
    // Clear the screen only for the daemon; a subcommand must not wipe the
    // user's terminal before printing its CSR / reply.
    let cfg = AppConfig::load(&cli.config, cli.command.is_none());

    // Utility subcommands run instead of the daemon (no endpoint, no wait).
    match cli.command {
        Some(cli::Command::ClearDb) => return control::clear_db_client(&cfg.control_socket).await,
        Some(cli::Command::Enroll) => return cmd::enroll(&cfg),
        None => {},
    }

    common::server::log::init(cfg.log.level.as_deref());
    crate::util::dht_log::DHT_LOG.store(cfg.log.dht, std::sync::atomic::Ordering::Relaxed);
    info!("pzrelay {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));

    // Block until we hold a valid cert (writes a CSR + waits if unenrolled), so
    // the endpoint is only built with usable TLS material.
    let csr_path = cfg.network.key_path.with_extension("csr");
    common::node::enroll::ensure_enrolled(&cfg.network, &csr_path, "relay").await?;
    if cfg.network.watch_reload {
        common::node::enroll::spawn_config_reload(cli.config.clone());
    }

    // `shutdown` (watch) feeds `ResolverLink`; `cancel` (token) fires every
    // `Acceptor` per-connection task. Both trip together on Ctrl-C.
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let cancel = CancellationToken::new();

    let control_sock = cfg.control_socket.clone();
    let relay = Arc::new(Relay::new(cfg));
    let acceptor = Acceptor::new(relay.endpoint.clone());

    let acceptor_handle = tokio::spawn({
        let relay = relay.clone();
        let cancel = cancel.clone();
        async move { acceptor.run(relay, cancel).await }
    });

    // Control socket for `pzrelay clear-db` (and future subcommands).
    tokio::spawn(control::serve(relay.store.clone(), control_sock, cancel.clone()));

    // Capture `client_handle` (Arc-shared, survives reconnects) before
    // `attach()` consumes the link — the DHT bootstrap RPCs need it.
    let resolver_link = ResolverLink::new(relay.clone(), shutdown_rx);
    let resolver_handle = resolver_link.client_handle();
    let resolver_attach_handle = resolver_link.attach();

    if let Some(dht) = relay.dht.clone()
        && dht.cfg.enabled {
            // Stash the resolver handle so the scheduler's retry branch and the
            // cold-start bootstrap below share one live session.
            dht.attach_resolver(resolver_handle.clone());

            // Keep the cached push-gateway directory fresh so `trigger_wake`
            // has targets. Detached; degrades to no-wakes when empty.
            tokio::spawn(crate::dht::push_wake::refresh_gateways(
                dht.clone(),
                resolver_handle.clone(),
            ));

            // Detached so a slow/absent resolver can't delay QUIC accept; on
            // failure the relay serves with an empty table until a retry wins.
            let resolver_handle_for_bootstrap = resolver_handle.clone();
            let dht_for_bootstrap = dht.clone();
            tokio::spawn(async move {
                match bootstrap::bootstrap(dht_for_bootstrap, resolver_handle_for_bootstrap).await {
                    Ok(state) => crate::dht_log!("DHT bootstrap reached state {state:?}"),
                    // Brand-new network is legitimate — info, not warn, so the
                    // first-relay operator isn't alarmed.
                    Err(bootstrap::BootstrapError::EmptyRegistry) => {
                        crate::dht_log!("DHT bootstrap: resolver returned no peers (new network?)")
                    },
                    Err(e) => crate::dht_log!("DHT bootstrap failed: {e}"),
                }
            });

            // Anti-entropy scheduler — runs even if bootstrap failed; degrades
            // gracefully on an empty table and retries bootstrap itself.
            let dht_for_sched = dht.clone();
            let cancel_for_sched = cancel.clone();
            tokio::spawn(async move {
                sync::run_scheduler(dht_for_sched, cancel_for_sched).await;
            });
        }

    tokio::select! {
        _ = acceptor_handle => {}
        _ = resolver_attach_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            // Cancel FIRST so per-connection tasks stop reading and can finish
            // in-flight fjall writes before the endpoint goes away.
            cancel.cancel();
            shutdown.send(()).ok();

            // Close DHT peers before the endpoint so in-flight `peer/1` RPCs
            // see a clean close-reason, not a transport error.
            if let Some(dht) = relay.dht.clone() {
                dht.shutdown().await;
            }

            relay.endpoint.close(CloseReason::ShuttingDown.code(), b"ShuttingDown");

            // Bounded flush window for in-flight frames (close, DispatchAcks,
            // Deliver) — a misbehaving peer can't stall shutdown past this.
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                relay.endpoint.wait_idle(),
            )
            .await;

            info!("closing relay!");
        }
    }

    Ok(())
}
