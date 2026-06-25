use std::net::SocketAddr;

use anyhow::Result;
use anyhow::anyhow;
use common::debug;
use common::info;
use common::node::config::NodeSeed;
use quinn::ClientConfig;
use quinn::Connection;
use tokio::task::JoinSet;

/// Try all seed resolvers concurrently and return the first successful
/// connection.
///
/// The dials run inside a [`JoinSet`], so dropping this future aborts every
/// in-flight dial *at its await point*. Two payoffs: once a winner is found
/// the losing dials are cancelled, and — the case that matters on shutdown —
/// when the resolver link races a shutdown signal and drops us mid-connect,
/// no detached dial survives to fail against the closing endpoint and log a
/// spurious error.
pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[NodeSeed], cfg: Option<ClientConfig>,
) -> Result<Connection> {
    if seeds.is_empty() {
        return Err(anyhow!("no resolver seeds provided"));
    }

    let mut dials: JoinSet<(SocketAddr, Result<Connection>)> = JoinSet::new();
    for seed in seeds {
        let endpoint = endpoint.clone();
        let addr = seed.addr;
        let key = seed.key.to_string();
        let cfg = cfg.clone();

        dials.spawn(async move {
            info!("connecting to resolver: {}", addr);
            let result = match cfg {
                Some(cfg) => match endpoint.connect_with(cfg, addr, &key) {
                    Ok(c) => c.await.map_err(anyhow::Error::from),
                    Err(e) => Err(e.into()),
                },
                None => match endpoint.connect(addr, &key) {
                    Ok(c) => c.await.map_err(anyhow::Error::from),
                    Err(e) => Err(e.into()),
                },
            };
            (addr, result)
        });
    }

    let mut last_err: Option<anyhow::Error> = None;
    while let Some(joined) = dials.join_next().await {
        // A panicked/aborted dial yields a JoinError — skip it, keep waiting.
        let Ok((addr, result)) = joined else { continue };
        match result {
            // Dropping `dials` here aborts the still-racing dials.
            Ok(conn) => {
                info!("connected to resolver: {}", addr);
                return Ok(conn);
            },
            // Per-seed detail at debug; the resolver-link loop surfaces the
            // final failure at warn ("resolver session ended … retrying").
            Err(e) => {
                debug!("resolver {} dial failed: {}", addr, e);
                last_err = Some(e);
            },
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
