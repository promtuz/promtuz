use std::sync::Arc;

use anyhow::Result;
use common::graceful;
use tokio::sync::Mutex;

use crate::quic::acceptor::Acceptor;
use crate::quic::resolver_link::ResolverLink;
use crate::relay::Relay;
use crate::relay::RelayRef;
use crate::util::config::AppConfig;

mod quic;
mod relay;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

    let relay: RelayRef = Arc::new(Mutex::new(Relay::new(cfg)));
    let acceptor = Acceptor::new(relay.lock().await.endpoint.clone());
    let resolver =
        Arc::new(graceful!(ResolverLink::new(relay.clone()).await, "RESOLVER_LINK_ERR:"));

    let acceptor_handle = tokio::spawn({
        let relay = relay.clone();
        async move { acceptor.run(relay.clone()).await }
    });

    let resolver_handle = tokio::spawn({
        let resolver = resolver.clone();
        async move { resolver.handle().await }
    });

    // Announcing Presence to Resolver
    resolver.hello().await?;

    tokio::select! {
        _ = resolver_handle => {}
        _ = acceptor_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            let relay = relay.lock().await;

            resolver.close();

            relay.endpoint.wait_idle().await;

            println!("CLOSING RELAY");
        }
    }

    Ok(())
}
