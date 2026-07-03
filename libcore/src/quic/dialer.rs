use std::io;
use std::time::Duration;

use common::node::config::DEFAULT_RESOLVER_PORT;
use quinn::Connection;
use thiserror::Error;

use crate::ENDPOINT;
use crate::data::ResolverSeed;

pub fn quinn_err<E>(e: E) -> DialerError
where
    E: std::error::Error + Send + Sync + 'static,
{
    DialerError::Quinn(Box::new(e))
}

#[derive(Error, Debug)]
pub enum DialerError {
    #[error("quinn failure: {0}")]
    Quinn(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("failed to connect: {0}")]
    Error(#[from] io::Error),
}

pub async fn connect_to_any_seed(seeds: &[ResolverSeed]) -> Result<Connection, DialerError> {
    let endpoint = ENDPOINT.get().unwrap();
    let mut last_err: Option<io::Error> = None;

    for seed in seeds {
        // Resolve host[:port] -> SocketAddr at dial time (DNS + default port),
        // so a DNS repoint is picked up on reconnect. A resolve failure just
        // moves to the next seed rather than aborting the whole attempt.
        let addr = match seed.addr.resolve(DEFAULT_RESOLVER_PORT).await {
            Ok(a) => a,
            Err(err) => {
                log::error!("ERROR: resolver {} resolve failed: {}", seed.addr, err);
                last_err = Some(err);
                continue;
            },
        };

        log::info!("INFO: connecting to resolver: {} ({})", seed.addr, addr);

        let connecting = endpoint.connect(addr, &seed.key.to_string()).map_err(quinn_err)?;
        match tokio::time::timeout(Duration::from_secs(10), connecting).await {
            Ok(Ok(conn)) => {
                log::info!("INFO: connected to resolver: {}", addr);
                return Ok(conn);
            },
            Ok(Err(err)) => {
                log::error!("ERROR: resolver {} connection failed: {}", addr, err);
                last_err = Some(err.into());
            },
            Err(_) => {
                log::warn!("WARN: resolver {} timed out after 10s", addr);
                last_err = Some(io::Error::new(io::ErrorKind::TimedOut, "resolver connect timed out"));
            },
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("no resolver seed succeeded")).into())
}
