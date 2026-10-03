//! Keeps a node registered in a resolver's directory and shares the session.

use std::convert::Infallible;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use quinn::Connection;
use quinn::Endpoint;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

use crate::node::config::DEFAULT_RESOLVER_PORT;
use crate::node::config::NodeSeed;
use crate::proto::pack::Unpacker as _;
use crate::proto::relay_res::LifetimeP;
use crate::proto::relay_res::NODE_HELLO_EXPORTER_LABEL;
use crate::proto::relay_res::ResolverPacket;
use crate::proto::relay_res::gateway_hello_signing_input;
use crate::proto::relay_res::relay_heartbeat_signing_input;
use crate::proto::relay_res::relay_hello_signing_input;
use crate::quic::CloseReason;
use crate::quic::RESOLVER_RELAY_HEARTBEAT_INTERVAL;
use crate::quic::id::NodeId;
use crate::quic::session_binding;
use crate::sysutils::system_load;
use crate::types::bytes::Bytes;
use crate::utils::now_ms;

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Relays also send signed heartbeats; a gateway's open connection is its liveness.
pub enum Hello {
    Relay,
    /// The gateway's CA-issued leaf, DER.
    Gateway(Vec<u8>),
}

/// The registered session; `None` while the link reconnects.
pub type Session = watch::Receiver<Option<Connection>>;

/// Runs until aborted. The endpoint's default client config must dial under the relay ALPN.
pub fn spawn(
    endpoint: Endpoint, seeds: Vec<NodeSeed>, key: SigningKey, hello: Hello,
) -> (Session, JoinHandle<()>) {
    let (registered, session) = watch::channel(None);
    let task = tokio::spawn(async move {
        if seeds.is_empty() {
            crate::warn!("no resolver seeds configured; this node is not discoverable");
            return std::future::pending::<()>().await;
        }
        let registered = Registered(registered);
        let mut delay = BACKOFF_MIN;
        loop {
            let Err(e) = register(&endpoint, &seeds, &key, &hello, &registered.0, &mut delay).await;
            registered.0.send_replace(None);
            crate::warn!("resolver session ended: {e:#}; retrying in {delay:?}");
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(BACKOFF_MAX);
        }
    });
    (session, task)
}

/// Aborting the link closes the session, rather than leaving it to whoever drops the last handle.
struct Registered(watch::Sender<Option<Connection>>);

impl Drop for Registered {
    fn drop(&mut self) {
        if let Some(conn) = self.0.send_replace(None) {
            CloseReason::ShuttingDown.close(&conn);
        }
    }
}

/// The backoff resets only once the resolver accepts the hello, so a rejected hello cannot
/// reconnect every second.
async fn register(
    endpoint: &Endpoint, seeds: &[NodeSeed], key: &SigningKey, hello: &Hello,
    registered: &watch::Sender<Option<Connection>>, delay: &mut Duration,
) -> Result<Infallible> {
    let conn = dial_any(endpoint, seeds).await?;
    let mut send = conn.open_uni().await?;
    hello.packet(key, &session_binding(&conn, NODE_HELLO_EXPORTER_LABEL)?).send(&mut send).await?;
    send.finish()?;
    let ResolverPacket::Lifetime(LifetimeP::HelloAck { .. }) =
        ResolverPacket::unpack(&mut conn.accept_uni().await?).await?
    else {
        bail!("resolver answered the hello without a HelloAck");
    };
    crate::info!("registered with resolver({})", conn.remote_address());
    *delay = BACKOFF_MIN;
    registered.send_replace(Some(conn.clone()));

    let started = Instant::now();
    let period = Duration::from_secs(RESOLVER_RELAY_HEARTBEAT_INTERVAL);
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        tokio::select! {
            closed = conn.closed() => return Err(closed.into()),
            _ = beat.tick(), if matches!(hello, Hello::Relay) => {
                heartbeat(&conn, key, started).await?
            },
        }
    }
}

impl Hello {
    fn packet(&self, key: &SigningKey, binding: &[u8; 32]) -> ResolverPacket {
        let pubkey = key.verifying_key().to_bytes();
        let id = NodeId::new(pubkey);
        let timestamp = u128::from(now_ms());
        let sign = |input: Vec<u8>| Bytes(key.sign(&input).to_bytes());
        ResolverPacket::Lifetime(match self {
            Hello::Relay => LifetimeP::RelayHello {
                relay_id: id,
                pubkey: Bytes(pubkey),
                timestamp,
                sig: sign(relay_hello_signing_input(&id, &pubkey, timestamp, binding)),
            },
            Hello::Gateway(cert) => LifetimeP::GatewayHello {
                gateway_id: id,
                pubkey: Bytes(pubkey),
                timestamp,
                sig: sign(gateway_hello_signing_input(&id, &pubkey, timestamp, binding)),
                cert: cert.clone(),
            },
        })
    }
}

async fn heartbeat(conn: &Connection, key: &SigningKey, started: Instant) -> Result<()> {
    let pubkey = key.verifying_key().to_bytes();
    let relay_id = NodeId::new(pubkey);
    let timestamp = u128::from(now_ms());
    let sig = key.sign(&relay_heartbeat_signing_input(&relay_id, &pubkey, timestamp)).to_bytes();
    let load = system_load().await;
    let mut send = conn.open_uni().await?;
    ResolverPacket::Lifetime(LifetimeP::RelayHeartbeat {
        relay_id,
        pubkey: Bytes(pubkey),
        timestamp,
        sig: Bytes(sig),
        load,
        uptime_seconds: started.elapsed().as_secs(),
    })
    .send(&mut send)
    .await?;
    Ok(send.finish()?)
}

/// The first seed to connect wins. Dropping the `JoinSet` aborts the other dials, so none
/// outlives a shutdown to fail against the closing endpoint.
async fn dial_any(endpoint: &Endpoint, seeds: &[NodeSeed]) -> Result<Connection> {
    let mut dials = JoinSet::new();
    for seed in seeds.iter().cloned() {
        let endpoint = endpoint.clone();
        dials.spawn(async move {
            let dial = async {
                let addr = seed.addr.resolve(DEFAULT_RESOLVER_PORT).await?;
                crate::info!("connecting to resolver: {} ({addr})", seed.addr);
                anyhow::Ok(endpoint.connect(addr, &seed.key.to_string())?.await?)
            };
            dial.await.with_context(|| format!("resolver {}", seed.addr))
        });
    }
    let mut last = anyhow!("no resolver seed succeeded");
    while let Some(joined) = dials.join_next().await {
        match joined.map_err(anyhow::Error::from).and_then(|dialed| dialed) {
            Ok(conn) => return Ok(conn),
            Err(e) => last = e,
        }
    }
    Err(last)
}
