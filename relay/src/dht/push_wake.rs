//! Offline-wake trigger. When the enqueue path durably stores a message for an
//! offline recipient, this asks a push gateway to wake the device.
//!
//! Best-effort by design: a failed wake is logged and dropped — the message is
//! already durably queued and delivers on the recipient's next foreground
//! drain. Nothing here is on the correctness path.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use common::debug;
use common::node::capability::NodeCapabilities;
use common::proto::client_res::GatewayDescriptor;
use common::proto::pack::Packer;
use common::proto::push::PushRequest;
use common::proto::push::WakeRequest;
use common::types::bytes::Bytes;
use quinn::Endpoint;

use super::Dht;
use crate::quic::resolver_link::ResolverLinkHandle;

impl Dht {
    /// Wake `recipient_ipk`'s device if we hold its pseudonym and know a
    /// gateway. No-op otherwise. Fire-and-forget: spawns the dial so the
    /// enqueue path never blocks on the gateway.
    pub(crate) fn trigger_wake(&self, recipient_ipk: &[u8; 32]) {
        let who = hex::encode(&recipient_ipk[..8]);
        let Some(endpoint) = &self.endpoint else {
            debug!("wake({who}) skipped: no DHT endpoint attached");
            return;
        };
        let Some(pseudonym) = self.store.get_push_pseudonym(recipient_ipk) else {
            debug!("wake({who}) skipped: no IPK→P mapping (recipient never registered a pseudonym here)");
            return;
        };
        // Pick a cached gateway (first). Empty → no wakes.
        let Some(gateway) = self.push_gateways.read().first().cloned() else {
            debug!("wake({who}) skipped: gateway directory empty (is a gateway registered with the resolver?)");
            return;
        };
        debug!("wake({who} P={}): dialing gateway {}", hex::encode(&pseudonym[..8]), gateway.id);

        let endpoint = endpoint.clone();
        tokio::spawn(async move {
            match send_wake(&endpoint, &gateway, pseudonym).await {
                Ok(()) => debug!("wake({who}): delivered to gateway"),
                Err(e) => debug!("wake({who}): failed: {e}"),
            }
        });
    }
}

/// Periodically refresh the cached gateway directory from the resolver.
pub(crate) async fn refresh_gateways(dht: Arc<Dht>, resolver: ResolverLinkHandle) {
    const REFRESH: Duration = Duration::from_secs(60);
    loop {
        match resolver.get_gateways().await {
            Ok(gws) => *dht.push_gateways.write() = gws,
            Err(e) => debug!("gateway refresh failed: {e}"),
        }
        tokio::time::sleep(REFRESH).await;
    }
}

/// Dial the gateway over `relay/1` (the endpoint's default client config),
/// verify it carries `PUSH_GATEWAY`, and send one [`WakeRequest`]. Contentless
/// payload — the device wakes and drains via the normal sticky-home path.
async fn send_wake(
    endpoint: &Endpoint, gateway: &GatewayDescriptor, pseudonym: [u8; 32],
) -> Result<()> {
    // ponytail: one QUIC dial per wake. Pool/cache the gateway connection if
    // wake volume ever makes the per-message handshake hurt.
    let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;

    // The resolver directory is untrusted — verify the dialed node's CA-signed
    // capability before handing it a pseudonym + wake.
    let caps = super::tls_extract::capabilities_from_conn(&conn)
        .ok_or_else(|| anyhow!("gateway cert carries no capability extension"))?;
    if !caps.contains(NodeCapabilities::PUSH_GATEWAY) {
        conn.close(0u32.into(), b"not-a-gateway");
        return Err(anyhow!("dialed {} lacks PUSH_GATEWAY", gateway.id));
    }

    let (mut send, _recv) = conn.open_bi().await?;
    let req = PushRequest::Wake(WakeRequest { pseudonym: Bytes(pseudonym), payload: Vec::new() });
    send.write_all(&req.pack()?).await?;
    send.finish()?;
    // finish() only marks the stream done locally; close() would drop the
    // still-in-flight frame before the gateway reads it. Await the gateway
    // consuming it first — same handshake the token-registration path uses
    // (libcore push::send_registration).
    let _ = send.stopped().await;
    conn.close(0u32.into(), b"wake-sent");
    Ok(())
}
