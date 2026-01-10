use std::sync::Arc;

use anyhow::Result;
use common::quic::CloseReason;
use tokio::sync::Mutex;
use tracing_log::LogTracer;
use tracing_subscriber::fmt;

use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::relay::RelayRef;
use crate::util::config::AppConfig;

mod dht;
mod quic;
mod relay;
mod util;

fn init_tracing() {
    LogTracer::builder()
        .with_max_level(log::LevelFilter::Trace)
        .init() // sets the `log` logger ONLY
        .ok();  // <- swallow "already set" instead of panic

    tracing::subscriber::set_global_default(
        fmt::Subscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    ).ok(); // same deal
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    
    let cfg = AppConfig::load(true);

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(());

    let relay: RelayRef = Arc::new(Mutex::new(Relay::new(cfg)));
    Relay::spawn_dht_tasks(relay.clone());

    let acceptor = Acceptor::new(relay.lock().await.endpoint.clone());

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

            let relay = relay.lock().await;

            shutdown.send(()).ok();

            relay.endpoint.close(CloseReason::ShuttingDown.code(), b"ShuttingDown");

            println!("CLOSING RELAY");
        }
    }

    Ok(())
}
