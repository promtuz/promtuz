use anyhow::Result;
use anyhow::anyhow;
use quinn::Connection;

use crate::data::ResolverSeed;

pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[ResolverSeed],
) -> Result<Connection> {
    // Collect errors to show if everything fails
    let mut last_err: Option<anyhow::Error> = None;

    for seed in seeds {
        let addr = seed.addr;

        log::info!("INFO: connecting to resolver: {}", addr);

        match endpoint.connect(addr, &seed.id.to_string())?.await {
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

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
