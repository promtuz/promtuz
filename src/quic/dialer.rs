use anyhow::Result;
use anyhow::anyhow;
use common::node::config::ResolverSeed;
use quinn::ClientConfig;
use quinn::Connection;

/// Try all seed resolvers and return the first successful connection.
pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[ResolverSeed], cfg: Option<ClientConfig>,
) -> Result<Connection> {
    // Collect errors to show if everything fails
    let mut last_err: Option<anyhow::Error> = None;

    let cfg = cfg.as_ref();
    for seed in seeds {
        let addr = seed.addr;

        println!("INFO: connecting to resolver: {}", addr);

        match if let Some(cfg) = cfg {
            endpoint.connect_with(cfg.clone(), addr, &seed.id.to_string())?.await
        } else {
            endpoint.connect(addr, &seed.id.to_string())?.await
        } {
            Ok(conn) => {
                println!("INFO: connected to resolver: {}", addr);
                return Ok(conn);
            },
            Err(err) => {
                eprintln!("ERROR: resolver {} connection failed: {}", addr, err);
                last_err = Some(err.into());
            },
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
