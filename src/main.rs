#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Result;
use tokio::join;
use tokio::sync::Mutex;

use crate::quic::acceptor::Acceptor;
use crate::resolver::Resolver;
use crate::util::config::AppConfig;

mod quic;
mod resolver;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = AppConfig::load(true);

    let resolver = Resolver::new(cfg);
    let acceptor = Acceptor::new(resolver.endpoint.clone());

    let resolver_arc = Arc::new(Mutex::new(resolver));

    let resolver = resolver_arc.clone();
    let acceptor_handle = tokio::spawn(async move { acceptor.run(resolver.clone()).await });

    _ = join!(acceptor_handle);

    Ok(())
}
