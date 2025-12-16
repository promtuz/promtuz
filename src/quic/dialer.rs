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

            match if let Some(cfg) = cfg {
                endpoint.connect_with(cfg.clone(), addr, &seed.id.to_string())?.await
            } else {
                endpoint.connect(addr, &seed.id.to_string())?.await
            } {
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
