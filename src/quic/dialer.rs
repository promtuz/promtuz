use anyhow::Result;
use anyhow::anyhow;
use quinn::Connection;
use url::Url;

/// Try all seed resolvers and return the first successful connection.
///
/// `host_name` is the SNI/hostname used for TLS validation.
pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[Url], host_name: &str,
) -> Result<Connection> {
    // Collect errors to show if everything fails
    let mut last_err: Option<anyhow::Error> = None;

    for seed in seeds {
        // Resolve URL → socket addresses
        let addrs = match seed.socket_addrs(|| None) {
            Ok(a) if !a.is_empty() => a,
            _ => {
                last_err = Some(anyhow!("failed to resolve seed: {}", seed));
                continue;
            },
        };

        // Try each resolved IP for this seed
        for addr in addrs {
            println!("RESOLVER: Trying to connect: {} ({})", seed, addr);
            match endpoint.connect(addr, host_name)?.await {
                Ok(conn) => {
                    println!("RESOLVER: Connected to: {} ({})", seed, addr);
                    return Ok(conn);
                },
                Err(err) => {
                    println!("ERROR: Failed to connect {} ({:?}): {}", seed, addr, err);
                    last_err = Some(err.into());
                },
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
