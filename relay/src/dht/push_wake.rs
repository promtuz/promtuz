//! Offline-wake trigger. When the enqueue path durably stores a message for an
//! offline recipient, this asks the push gateway to wake the device.
//!
//! Best-effort by design: a failed wake is logged and dropped — the message is
//! already durably queued and delivers on the recipient's next foreground
//! drain. Nothing here is on the correctness path.

use anyhow::Result;
use common::debug;
use common::node::config::DEFAULT_GATEWAY_PORT;
use common::node::config::NodeSeed;
use common::proto::pack::Packer;
use common::proto::push::PushRequest;
use common::proto::push::WakeRequest;
use common::types::bytes::Bytes;
use quinn::Endpoint;

use super::Dht;

impl Dht {
    /// Wake `recipient_ipk`'s device if we hold its pseudonym and a gateway is
    /// configured. No-op otherwise. Fire-and-forget: spawns the dial so the
    /// enqueue path never blocks on the gateway.
    pub(crate) fn trigger_wake(&self, recipient_ipk: &[u8; 32]) {
        let (Some(map), Some(gateway), Some(endpoint)) =
            (&self.push_pseudonyms, &self.push_gateway, &self.endpoint)
        else {
            return;
        };
        let Some(pseudonym) = map.read().get(recipient_ipk).copied() else {
            return; // this relay isn't a home the device registered with
        };

        let endpoint = endpoint.clone();
        let gateway = gateway.clone();
        tokio::spawn(async move {
            if let Err(e) = send_wake(&endpoint, &gateway, pseudonym).await {
                debug!("push wake failed: {e}");
            }
        });
    }
}

/// Dial the gateway over `relay/1` (the endpoint's default client config) and
/// send one [`WakeRequest`]. Contentless payload — the device wakes and drains
/// via the normal sticky-home path. (Carrying the ciphertext envelope inline is
/// a later size-branch optimisation.)
async fn send_wake(endpoint: &Endpoint, gateway: &NodeSeed, pseudonym: [u8; 32]) -> Result<()> {
    let addr = gateway.addr.resolve(DEFAULT_GATEWAY_PORT).await?;
    // ponytail: one QUIC dial per wake. Pool/cache the gateway connection if
    // wake volume ever makes the per-message handshake hurt.
    let conn = endpoint.connect(addr, &gateway.key.to_string())?.await?;
    let (mut send, _recv) = conn.open_bi().await?;
    let req = PushRequest::Wake(WakeRequest { pseudonym: Bytes(pseudonym), payload: Vec::new() });
    send.write_all(&req.pack()?).await?;
    send.finish()?;
    // Let the stream flush before the connection drops.
    conn.close(0u32.into(), b"wake-sent");
    Ok(())
}
