use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::info;
use common::quic::CloseReason;
use tokio_util::sync::CancellationToken;

use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::util::config::AppConfig;

mod quic;
mod relay;
mod storage;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

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

    let resolver_handle = ResolverLink::new(relay.clone(), shutdown_rx).attach();

    tokio::select! {
        _ = acceptor_handle => {}
        _ = resolver_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            // Signal cooperative shutdown FIRST so per-connection tasks
            // stop reading new packets and can finish in-flight RocksDB
            // batches before the endpoint goes away.
            cancel.cancel();
            shutdown.send(()).ok();

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
