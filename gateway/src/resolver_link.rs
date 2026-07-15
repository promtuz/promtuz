//! Keeps the gateway in the resolver's directory: dial a resolver seed, send a
//! signed `GatewayHello`, and hold the connection as the liveness signal.
//! Reconnect + re-hello on drop. No heartbeat — the live connection *is* the
//! liveness (the resolver evicts on close).

use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use anyhow::anyhow;
use common::info;
use common::node::config::DEFAULT_RESOLVER_PORT;
use common::node::config::NodeSeed;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::proto::relay_res::gateway_hello_signing_input;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use common::warn;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use quinn::Connection;
use quinn::Endpoint;

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// Spawn the resolver-registration loop. No-op (with a warning) if no resolver
/// seeds are configured — the gateway still runs, just undiscoverable.
pub fn spawn(endpoint: Endpoint, seeds: Vec<NodeSeed>, signing: SigningKey, node_id: NodeId) {
    if seeds.is_empty() {
        warn!("no resolver seeds configured — gateway will not be discoverable");
        return;
    }
    tokio::spawn(async move {
        loop {
            match register(&endpoint, &seeds, &signing, node_id).await {
                Ok(conn) => {
                    info!("registered with resolver({})", conn.remote_address());
                    let _ = conn.closed().await;
                    warn!("resolver session ended; reconnecting");
                },
                Err(e) => warn!("resolver registration failed: {e}"),
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    });
}

async fn register(
    endpoint: &Endpoint, seeds: &[NodeSeed], signing: &SigningKey, node_id: NodeId,
) -> Result<Connection> {
    let conn = dial_any(endpoint, seeds).await?;

    let pubkey = signing.verifying_key().to_bytes();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let sig = signing.sign(&gateway_hello_signing_input(&node_id, &pubkey, ts)).to_bytes();
    let hello = ResolverPacket::Lifetime(LifetimeP::GatewayHello {
        gateway_id: node_id,
        pubkey:     Bytes(pubkey),
        timestamp:  ts,
        sig:        Bytes(sig),
    });

    let mut send = conn.open_uni().await?;
    hello.send(&mut send).await?;
    send.finish()?;
    Ok(conn)
}

/// Dial resolver seeds in order; first success wins. Uses the endpoint's
/// default client config (`relay/1`).
async fn dial_any(endpoint: &Endpoint, seeds: &[NodeSeed]) -> Result<Connection> {
    let mut last: Option<anyhow::Error> = None;
    for seed in seeds {
        let addr = match seed.addr.resolve(DEFAULT_RESOLVER_PORT).await {
            Ok(a) => a,
            Err(e) => {
                last = Some(anyhow!("resolve {}: {e}", seed.addr));
                continue;
            },
        };
        match endpoint.connect(addr, &seed.key.to_string()) {
            Ok(c) => match c.await {
                Ok(conn) => return Ok(conn),
                Err(e) => last = Some(e.into()),
            },
            Err(e) => last = Some(e.into()),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no resolver seeds")))
}
