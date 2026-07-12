//! Offline-wake registration. Mints a push-pseudonym `P` (a random Ed25519
//! keypair, unrelated to the IPK) and tells the home relay `IPK → P`, so the
//! relay can wake this device when a message queues while we're offline. The
//! device token never touches the relay — only the gateway learns it, under
//! `P` (that half is a separate registration).

use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_rel::CRelayPacket;
use common::proto::pack::Packer;
use common::types::bytes::Bytes;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use once_cell::sync::Lazy;

use crate::state::RELAY;

/// The push-pseudonym keypair. Random and *not* derived from the IPK (so the
/// gateway can't link `P` back to us), and — because it's per-install, not
/// per-identity — distinct on each device sharing one identity.
///
// ponytail: process-lifetime only. Persist the seed via SecureStore for a
// pseudonym that survives restarts, instead of registering a fresh `P` each
// launch and orphaning the old one.
static PUSH_KEY: Lazy<SigningKey> = Lazy::new(|| {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    SigningKey::from_bytes(&seed)
});

/// Our push-pseudonym `P` (the public half) — also what verifies the
/// gateway-side `RegisterToken`.
pub fn push_pseudonym() -> [u8; 32] {
    PUSH_KEY.verifying_key().to_bytes()
}

/// Tell the connected home relay our `P`. Fire-and-forget; the relay binds it
/// to the connection-authenticated IPK. Called on each relay connect.
pub async fn register_push() -> Result<()> {
    let bytes = CRelayPacket::RegisterPush { pseudonym: Bytes(push_pseudonym()) }
        .pack()
        .map_err(|e| anyhow!("pack register_push: {e}"))?;
    let conn = {
        let relay = RELAY.read();
        relay.as_ref().and_then(|r| r.connection.clone())
    };
    let Some(conn) = conn else { return Ok(()) };
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = tx.write_all(&bytes).await;
        let _ = tx.finish();
    }
    Ok(())
}
