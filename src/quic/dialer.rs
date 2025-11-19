use anyhow::Result;
use anyhow::anyhow;
use common::node::config::ResolverSeed;
use quinn::Connection;

/// Try all seed resolvers and return the first successful connection.
pub async fn connect_to_any_seed(endpoint: &quinn::Endpoint, seeds: &[ResolverSeed]) -> Result<Connection> {
    // Collect errors to show if everything fails
    let mut last_err: Option<anyhow::Error> = None;

    for seed in seeds {
        // Resolve URL → socket addresses
        let addrs = match seed.url.socket_addrs(|| None) {
            Ok(a) if !a.is_empty() => a,
            _ => {
                last_err = Some(anyhow!("failed to resolve seed: {}", seed.url));
                continue;
            },
        };

        // Try each resolved IP for this seed
        for addr in addrs {
            println!("RESOLVER: Trying to connect: {} ({})", seed.url, addr);
            match endpoint.connect(addr, &seed.id.to_string())?.await {
                Ok(conn) => {
                    println!("RESOLVER: Connected to: {} ({})", seed.url, addr);
                    return Ok(conn);
                },
                Err(err) => {
                    println!("ERROR: Failed to connect {} ({:?}): {}", seed.url, addr, err);
                    last_err = Some(err.into());
                },
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
