//! Asks a push gateway to wake an offline recipient's device. Best effort: the message is already
//! durably queued, so a failed wake is only logged.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use anyhow::anyhow;
use common::debug;
use common::node::capability::NodeCapabilities;
use common::proto::client_rel::Wake;
use common::proto::client_res::GatewayDescriptor;
use common::proto::pack::Packer;
use common::proto::push::GatewayRequest;
use common::proto::push::MAX_PUSH_GATEWAYS;
use common::proto::RelayId;
use common::proto::push::WakeRequest;
use common::types::bytes::Bytes;
use governor::Quota;
use quinn::Connection;
use quinn::Endpoint;
use tokio::task::JoinSet;
use tokio::time::timeout;

use super::Dht;
use super::rate_limit::KeyedLimiter;
use super::rate_limit::LimiterClock;
use super::rate_limit::keyed;
use crate::quic::resolver_link::ResolverLinkHandle;

/// Sustained and burst wake budget per recipient. A wake costs a QUIC dial, so
/// the enqueue path cannot be allowed to mint one per injected dispatch.
const MAX_WAKES_PER_RECIPIENT_PER_HOUR: u32 = 120;
const MAX_WAKE_BURST: u32 = 12;

const WAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn wake_limiter(clock: &LimiterClock) -> KeyedLimiter<[u8; 32]> {
    let period = Duration::from_secs(3600 / MAX_WAKES_PER_RECIPIENT_PER_HOUR as u64);
    let quota = Quota::with_period(period)
        .expect("non-zero period per token")
        .allow_burst(NonZeroU32::new(MAX_WAKE_BURST).unwrap_or(NonZeroU32::MIN));
    keyed(quota, clock)
}

impl Dht {
    pub(crate) fn trigger_wake(self: &Arc<Self>, recipient_ipk: &[u8; 32], class: Wake) {
        let who = hex::encode(&recipient_ipk[..8]);
        if self.endpoint.is_none() {
            debug!("wake({who}) skipped: no DHT endpoint attached");
            return;
        }
        let Some(pseudonym) = self.store.get_push_pseudonym(recipient_ipk) else {
            debug!("wake({who}) skipped: no IPK→P mapping (recipient never registered a pseudonym here)");
            return;
        };
        if self.wake_limiter.check_key(recipient_ipk).is_err() {
            debug!("wake({who}) skipped: per-recipient wake quota exhausted");
            return;
        }
        let mut gateways = self.push_gateways.read().clone();
        if gateways.is_empty() {
            debug!("wake({who}) skipped: gateway directory empty (is a gateway registered with the resolver?)");
            return;
        }
        gateways.sort_by_key(|g| g.id);
        gateways.truncate(MAX_PUSH_GATEWAYS);
        debug!(
            "wake({who} P={}): dialing {} gateway(s)",
            hex::encode(&pseudonym[..8]),
            gateways.len()
        );

        let dht = self.clone();
        tokio::spawn(async move {
            for gateway in &gateways {
                match timeout(WAKE_TIMEOUT, send_wake(&dht, gateway, pseudonym, class)).await {
                    Ok(Ok(())) => debug!("wake({who}): delivered to gateway {}", gateway.id),
                    Ok(Err(e)) => debug!("wake({who}): gateway {} failed: {e}", gateway.id),
                    Err(_) => debug!("wake({who}): gateway {} timed out", gateway.id),
                }
            }
        });
    }
}

/// The resolver admits any self-made key, so only gateways whose certificate carries the capability
/// are kept, and the cap on wake targets applies to those.
pub(crate) async fn refresh_gateways(dht: Arc<Dht>, resolver: ResolverLinkHandle) {
    const REFRESH: Duration = Duration::from_secs(60);
    const RECHECK_VERIFIED: Duration = Duration::from_secs(3600);
    const RECHECK_REFUSED: Duration = Duration::from_secs(600);
    let mut verdicts: HashMap<RelayId, (bool, Instant)> = HashMap::new();
    loop {
        match (resolver.get_gateways().await, dht.endpoint.clone()) {
            (Ok(directory), Some(endpoint)) => {
                let now = Instant::now();
                verdicts.retain(|_, (ok, at)| {
                    now.duration_since(*at) < if *ok { RECHECK_VERIFIED } else { RECHECK_REFUSED }
                });
                let mut checks = JoinSet::new();
                for gateway in directory.iter().filter(|g| !verdicts.contains_key(&g.id)).cloned() {
                    let endpoint = endpoint.clone();
                    checks.spawn(async move {
                        let ok = timeout(WAKE_TIMEOUT, dial_gateway(&endpoint, &gateway))
                            .await
                            .is_ok_and(|dialed| dialed.map(|c| c.close(0u32.into(), b"verified")).is_ok());
                        (gateway.id, ok)
                    });
                }
                while let Some(Ok((id, ok))) = checks.join_next().await {
                    verdicts.insert(id, (ok, Instant::now()));
                }
                let verified: Vec<GatewayDescriptor> = directory
                    .into_iter()
                    .filter(|g| verdicts.get(&g.id).is_some_and(|(ok, _)| *ok))
                    .collect();
                *dht.push_gateways.write() = verified;
            },
            (Ok(_), None) => debug!("gateway refresh skipped: no DHT endpoint attached"),
            (Err(e), _) => debug!("gateway refresh failed: {e}"),
        }
        tokio::time::sleep(REFRESH).await;
    }
}

/// Dial a gateway and prove it is one: the directory is untrusted, the CA
/// stamp on its leaf certificate is not.
async fn dial_gateway(endpoint: &Endpoint, gateway: &GatewayDescriptor) -> Result<Connection> {
    let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;
    let caps = super::tls_extract::capabilities_from_conn(&conn)
        .ok_or_else(|| anyhow!("gateway cert carries no capability extension"))?;
    if !caps.contains(NodeCapabilities::PUSH_GATEWAY) {
        conn.close(0u32.into(), b"not-a-gateway");
        return Err(anyhow!("dialed {} lacks PUSH_GATEWAY", gateway.id));
    }
    Ok(conn)
}

/// The payload is empty: the device wakes and drains through its home.
async fn send_wake(
    dht: &Dht, gateway: &GatewayDescriptor, pseudonym: [u8; 32], class: Wake,
) -> Result<()> {
    let conn = gateway_conn(dht, gateway).await?;
    let (mut send, _recv) = conn.open_bi().await?;
    let req = GatewayRequest::Wake(WakeRequest {
        pseudonym: Bytes(pseudonym),
        payload: Vec::new(),
        class,
    });
    send.write_all(&req.pack()?).await?;
    send.finish()?;
    // finish() only marks the stream done locally; the gateway has to have
    // taken the frame before this counts as delivered.
    send.stopped().await?;
    Ok(())
}

async fn gateway_conn(dht: &Dht, gateway: &GatewayDescriptor) -> Result<Connection> {
    let cached = dht.gateway_conns.read().get(&gateway.id).cloned();
    if let Some(conn) = cached
        && conn.close_reason().is_none()
    {
        return Ok(conn);
    }
    let endpoint = dht.endpoint.as_ref().ok_or_else(|| anyhow!("no DHT endpoint attached"))?;
    let conn = dial_gateway(endpoint, gateway).await?;
    dht.gateway_conns.write().insert(gateway.id, conn.clone());
    Ok(conn)
}
