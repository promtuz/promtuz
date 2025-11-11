#![deny(clippy::expect_used, clippy::panic, clippy::indexing_slicing)]
#![warn(clippy::unwrap_used)]
#![forbid(unsafe_code)]

use std::sync::Arc;

use anyhow::Ok;
use anyhow::Result;
use tokio::join;

use crate::quic::acceptor::run_acceptor;
use crate::quic::endpoint::QuicEndpoint;
use crate::util::cls;
use crate::util::config::AppConfig;

mod quic;
mod router;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
    cls();

    let config = AppConfig::load();

    let endpoint = Arc::new(QuicEndpoint::new(&config)?);

    let acceptor_handle = tokio::spawn(run_acceptor(endpoint.clone()));

    _ = join!(acceptor_handle);
    Ok(())
}
