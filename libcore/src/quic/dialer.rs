use std::io;

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
        let addr = seed.addr;

        log::info!("INFO: connecting to resolver: {}", addr);

        match endpoint.connect(addr, &seed.id.to_string()).map_err(quinn_err)?.await {
            Ok(conn) => {
                log::info!("INFO: connected to resolver: {}", addr);
                return Ok(conn);
            },
            Err(err) => {
                log::error!("ERROR: resolver {} connection failed: {}", addr, err);
                last_err = Some(err.into());
            },
        }
    }

    Err(last_err.unwrap_or_else(|| io::Error::other("no resolver seed succeeded")).into())
}
