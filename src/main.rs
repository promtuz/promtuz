#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

mod proto;
mod quic;
mod resolver;
mod util;

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::quic::acceptor::Acceptor;
use crate::resolver::Resolver;
use crate::util::config::AppConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

    let resolver = Arc::new(Mutex::new(Resolver::new(cfg)));
    let acceptor = Acceptor::new(resolver.lock().await.endpoint.clone());

    let acceptor_handle = tokio::spawn({
        let resolver = resolver.clone();
        async move { acceptor.run(resolver.clone()).await }
    });

    tokio::select! {
        _ = acceptor_handle => {}
        _ = tokio::signal::ctrl_c() => {
            println!();

            let r = resolver.lock().await;
            r.close();
            r.endpoint.wait_idle().await;

            println!("CLOSING RESOLVER");
        }
    }

    Ok(())
}
