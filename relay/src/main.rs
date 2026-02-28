use std::sync::Arc;

use anyhow::Result;
use common::info;
use common::quic::CloseReason;

use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::util::config::AppConfig;

mod quic;
mod relay;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());

    // let relay: RelayRef = Arc::new(Mutex::new(Relay::new(cfg)));
    let relay = Arc::new(Relay::new(cfg));

    let acceptor = Acceptor::new(relay.endpoint.clone());

    let acceptor_handle = tokio::spawn({
        let relay = relay.clone();
        async move { acceptor.run(relay.clone()).await }
    });

    let resolver_handle = ResolverLink::attach(relay.clone(), shutdown_rx).await;

    tokio::select! {
        _ = acceptor_handle => {}
        _ = resolver_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            shutdown.send(()).ok();

            relay.endpoint.close(CloseReason::ShuttingDown.code(), b"ShuttingDown");

            info!("closing relay!");
        }
    }

    Ok(())
}
