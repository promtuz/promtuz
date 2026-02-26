use anyhow::Result;
use anyhow::anyhow;
use common::info;
use common::node::config::NodeSeed;
use quinn::ClientConfig;
use quinn::Connection;

/// Try all seed resolvers and return the first successful connection.
/// TODO: implement concurrent connection trial
pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[NodeSeed], cfg: Option<ClientConfig>,
) -> Result<Connection> {
    let mut last_err: Option<anyhow::Error> = None;

    let cfg = cfg.as_ref();
    for seed in seeds {
        let addr = seed.addr;

        info!("connecting to resolver: {}", addr);

        match if let Some(cfg) = cfg {
            endpoint.connect_with(cfg.clone(), addr, &seed.key.to_string())?.await
        } else {
            endpoint.connect(addr, &seed.key.to_string())?.await
        } {
            Ok(conn) => {
                info!("connected to resolver: {}", addr);
                return Ok(conn);
            },
            Err(err) => {
                common::error!("resolver {} connection failed: {}", addr, err);
                last_err = Some(err.into());
            },
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
