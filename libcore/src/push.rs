//! Offline-wake registration. Mints a push-pseudonym `P` (a random Ed25519
//! keypair, unrelated to the IPK) and tells the home relay `IPK → P`, so the
//! relay can wake this device when a message queues while we're offline. The
//! device token never touches the relay — only the gateway learns it, under
//! `P` (that half is a separate registration).

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::GatewayDescriptor;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::PushProvider;
use common::proto::push::PushRequest;
use common::proto::push::RegisterToken;
use common::types::bytes::Bytes;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use once_cell::sync::Lazy;

use crate::ENDPOINT;
use crate::RESOLVER_SEEDS;
use crate::quic::dialer::connect_to_any_seed;
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

/// The platform push token (e.g. FCM registration token), pushed in by the app
/// from its onNewToken callback. Registered with a gateway under `P`.
static PUSH_TOKEN: parking_lot::RwLock<Option<Vec<u8>>> = parking_lot::RwLock::new(None);

/// Store the platform push token and register `P → token` with a gateway.
pub async fn set_push_token(token: Vec<u8>) {
    *PUSH_TOKEN.write() = Some(token);
    if let Err(e) = register_token_at_gateway().await {
        log::debug!("PUSH: token registration failed: {e}");
    }
}

/// Register `P → token` with a discovered gateway, if we hold a token. Dials
/// the gateway *directly* (client/1) so the relay never learns the token, and
/// self-signs with `P` so the gateway never learns the IPK. No-op without a
/// token. Also (re)runs on relay connect.
pub async fn register_token_at_gateway() -> Result<()> {
    let Some(token) = PUSH_TOKEN.read().clone() else {
        return Ok(());
    };
    let gateway = fetch_gateway().await?;

    // ponytail: Fcm-only for now (Android). Pass the provider from the app when
    // iOS / UnifiedPush land.
    let reg = RegisterToken::signed(&PUSH_KEY, PushProvider::Fcm, token);
    let endpoint = ENDPOINT.get().context("endpoint not initialized")?;
    let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;
    let (mut tx, _rx) = conn.open_bi().await?;
    tx.write_all(&PushRequest::Register(reg).pack()?).await?;
    tx.finish()?;
    conn.close(0u32.into(), b"registered");
    Ok(())
}

/// Ask a resolver for a push gateway to register with; returns the first one.
async fn fetch_gateway() -> Result<GatewayDescriptor> {
    let seeds = RESOLVER_SEEDS.get().context("resolver seeds not set")?;
    let conn = connect_to_any_seed(seeds).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&ClientRequest::GetGateways().pack()?).await?;
    send.finish()?;
    let resp = ClientResponse::unpack(&mut recv).await?;
    conn.close(0u32.into(), b"done");
    match resp {
        ClientResponse::GetGateways { gateways } => {
            gateways.into_iter().next().context("no gateways registered")
        },
        other => Err(anyhow!("GetGateways: unexpected variant {other:?}")),
    }
}
