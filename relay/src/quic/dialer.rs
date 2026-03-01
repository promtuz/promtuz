use anyhow::Result;
use anyhow::anyhow;
use common::info;
use common::node::config::NodeSeed;
use quinn::ClientConfig;
use quinn::Connection;

/// Try all seed resolvers concurrently and return the first successful connection.
pub async fn connect_to_any_seed(
    endpoint: &quinn::Endpoint, seeds: &[NodeSeed], cfg: Option<ClientConfig>,
) -> Result<Connection> {
    if seeds.is_empty() {
        return Err(anyhow!("no resolver seeds provided"));
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Connection, anyhow::Error>>(seeds.len());

    let mut handles = Vec::with_capacity(seeds.len());

    for seed in seeds {
        let tx = tx.clone();
        let endpoint = endpoint.clone();
        let addr = seed.addr;
        let key = seed.key.to_string();
        let cfg = cfg.clone();

        handles.push(tokio::spawn(async move {
            info!("connecting to resolver: {}", addr);

            let result = match cfg.as_ref() {
                Some(cfg) => match endpoint.connect_with(cfg.clone(), addr, &key) {
                    Ok(connecting) => connecting.await.map_err(anyhow::Error::from),
                    Err(e) => Err(e.into()),
                },
                None => match endpoint.connect(addr, &key) {
                    Ok(connecting) => connecting.await.map_err(anyhow::Error::from),
                    Err(e) => Err(e.into()),
                },
            };

            if let Err(ref err) = result {
                common::error!("resolver {} connection failed: {}", addr, err);
            } else {
                info!("connected to resolver: {}", addr);
            }

            let _ = tx.send(result).await;
        }));
    }

    // Drop the original sender so the channel closes when all tasks finish.
    drop(tx);

    let mut last_err: Option<anyhow::Error> = None;
    let mut remaining = seeds.len();

    while let Some(result) = rx.recv().await {
        remaining -= 1;
        match result {
            Ok(conn) => {
                // Abort remaining tasks — we have what we need.
                for h in handles {
                    h.abort();
                }
                return Ok(conn);
            },
            Err(e) => {
                last_err = Some(e);
                if remaining == 0 {
                    break;
                }
            },
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no resolver seed succeeded")))
}
