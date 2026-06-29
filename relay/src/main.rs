use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::info;
use common::quic::CloseReason;
use common::warn;
use tokio_util::sync::CancellationToken;

use crate::dht::bootstrap;
use crate::dht::sync;
use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::util::config::AppConfig;

mod cli;
mod dht;
mod quic;
mod relay;
mod storage;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::get();

    let cfg = AppConfig::load(&cli.config, true);
    common::server::log::init(cfg.log.level.as_deref());
    info!("pzrelay {} ({})", env!("CARGO_PKG_VERSION"), env!("PZ_GIT_SHA"));

    // Hold here until we have a valid cert for our key (writes a CSR + waits
    // if not enrolled), so the endpoint is only built with usable TLS material.
    let csr_path = cfg.network.key_path.with_extension("csr");
    common::node::enroll::ensure_enrolled(&cfg.network, &csr_path, "relay").await?;
    if cfg.network.watch_reload {
        common::node::enroll::spawn_config_reload(cli.config.clone());
    }

    // `shutdown` is the legacy `watch` channel still consumed by
    // `ResolverLink`; `cancel` is the unified token observed by every
    // per-connection task spawned via `Acceptor`. Both fire together on
    // Ctrl-C — the watch channel could be retired once `ResolverLink`
    // adopts `CancellationToken`, but that's outside this change's scope.
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());
    let cancel = CancellationToken::new();

    // let relay: RelayRef = Arc::new(Mutex::new(Relay::new(cfg)));
    let relay = Arc::new(Relay::new(cfg));
    let acceptor = Acceptor::new(relay.endpoint.clone());

    let acceptor_handle = tokio::spawn({
        let relay = relay.clone();
        let cancel = cancel.clone();
        async move { acceptor.run(relay, cancel).await }
    });

    // Construct the resolver link, but capture its `client_handle` for
    // ad-hoc RPCs (DHT bootstrap) *before* `attach()` consumes the
    // `ResolverLink`. The handle is internally `Arc`-shared with the
    // attached task, so it stays in sync as the session reconnects.
    let resolver_link = ResolverLink::new(relay.clone(), shutdown_rx);
    let resolver_handle = resolver_link.client_handle();
    let resolver_attach_handle = resolver_link.attach();

    // DHT bootstrap — feature-gated on `cfg.dht.enabled`. Spawned as
    // a detached task so a slow/unavailable resolver does not delay
    // QUIC accept. Failures are logged and swallowed; the relay keeps
    // serving client traffic with an empty routing table until a
    // future bootstrap attempt succeeds (the scheduler retries
    // periodically).
    if let Some(dht) = relay.dht.clone()
        && dht.cfg.enabled {
            // Stash the resolver handle on `Dht` so the scheduler's
            // bootstrap-retry branch can re-call `bootstrap()` when the
            // routing table is sparse. The initial cold-start bootstrap
            // below uses the same handle — passed by clone so both code
            // paths see the same live session.
            dht.attach_resolver(resolver_handle.clone());

            let resolver_handle_for_bootstrap = resolver_handle.clone();
            let dht_for_bootstrap = dht.clone();
            tokio::spawn(async move {
                match bootstrap::bootstrap(dht_for_bootstrap, resolver_handle_for_bootstrap).await {
                    Ok(state) => info!("DHT bootstrap reached state {state:?}"),
                    // EmptyRegistry is the legitimate brand-new-network
                    // case — log at info, not warn, so an operator
                    // standing up the first relay isn't alarmed.
                    Err(bootstrap::BootstrapError::EmptyRegistry) => {
                        info!("DHT bootstrap: resolver returned no peers (new network?)")
                    },
                    Err(e) => warn!("DHT bootstrap failed: {e}"),
                }
            });

            // Anti-entropy + maintenance scheduler. Cooperative with
            // the cancellation token so Ctrl-C exits cleanly within
            // one cadence-tick. Spawned regardless of whether
            // bootstrap succeeds — the scheduler degrades gracefully
            // under empty routing tables (the bootstrap-retry branch
            // takes over once a resolver session becomes live).
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

            // Signal cooperative shutdown FIRST so per-connection tasks
            // stop reading new packets and can finish in-flight fjall
            // writes before the endpoint goes away.
            cancel.cancel();
            shutdown.send(()).ok();

            // Tear down DHT peer connections before closing the endpoint,
            // so in-flight `peer/1` RPCs see a clean close-reason rather
            // than a transport error. (Best-effort: the close frames go
            // out as part of the endpoint flush below.)
            if let Some(dht) = relay.dht.clone() {
                dht.shutdown().await;
            }

            relay.endpoint.close(CloseReason::ShuttingDown.code(), b"ShuttingDown");

            // Give in-flight QUIC frames (close frames, last DispatchAcks,
            // pending Deliver frames) a brief window to flush. Same
            // pattern as the resolver — bounded so a misbehaving peer
            // can't stall shutdown indefinitely.
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
