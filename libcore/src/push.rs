//! Offline-wake registration. Mints a push-pseudonym `P` (a random Ed25519
//! keypair, unrelated to the IPK) and tells the home relay `IPK → P`, so the
//! relay can wake this device when a message queues while we're offline. The
//! device token never touches the relay — only the gateway learns it, under
//! `P` (that half is a separate registration).

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use common::proto::RelayId;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::GatewayDescriptor;
use common::proto::dht_p2p::push_pseudonym_signing_input;
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
use rusqlite::params;

use crate::ENDPOINT;
use crate::RESOLVER_SEEDS;
use crate::data::identity::IdentitySigner;
use crate::db::network::NETWORK_DB;
use crate::quic::dialer::connect_to_any_seed;
use crate::state::RELAY;

/// The push-pseudonym keypair. Random and *not* derived from the IPK (so the
/// gateway can't link `P` back to us), and — because it's per-install, not
/// per-identity — distinct on each device sharing one identity.
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
    let pseudonym = push_pseudonym();
    let timestamp = now_ms();
    let ipk = crate::data::identity::Identity::get().context("no identity")?.ipk();
    let sig = IdentitySigner::sign(&push_pseudonym_signing_input(&ipk, &pseudonym, timestamp))?
        .to_bytes();
    let bytes =
        CRelayPacket::RegisterPush { pseudonym: Bytes(pseudonym), timestamp, sig: Bytes(sig) }
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    // Gateway registration dials over the shared endpoint (and `fetch_gateway`
    // may dial the resolver, which `.unwrap()`s it). During cold start the FCM
    // token callback can win the race against `init()` setting ENDPOINT, so skip
    // quietly if it isn't up yet — the relay-connect path re-runs this once it is.
    if ENDPOINT.get().is_none() {
        return Ok(());
    }
    let Some(token) = PUSH_TOKEN.read().clone() else {
        return Ok(());
    };
    let gateway = fetch_gateway().await?;
    if let Err(e) = send_registration(&gateway, token).await {
        // A dead cached gateway would otherwise wedge registration forever;
        // evicting it forces the next call back through the resolver.
        evict_gateway(&gateway.id);
        return Err(e);
    }
    Ok(())
}

async fn send_registration(gateway: &GatewayDescriptor, token: Vec<u8>) -> Result<()> {
    // ponytail: Fcm-only for now (Android). Pass the provider from the app when
    // iOS / UnifiedPush land.
    let reg = RegisterToken::signed(&PUSH_KEY, PushProvider::Fcm, token);
    let endpoint = ENDPOINT.get().context("endpoint not initialized")?;
    let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;
    let (mut tx, _rx) = conn.open_bi().await?;
    tx.write_all(&PushRequest::Register(reg).pack()?).await?;
    tx.finish()?;
    // finish() only marks the stream done locally; await the peer's ack or close() drops the unsent
    // op.
    let _ = tx.stopped().await;
    conn.close(0u32.into(), b"registered");
    Ok(())
}

/// Return a cached gateway if one is stored; else ask a resolver, cache the
/// result, and return the first. Mirrors the relay cache: the resolver is
/// dialed only on a miss.
async fn fetch_gateway() -> Result<GatewayDescriptor> {
    if let Some(gateway) = cached_gateway() {
        return Ok(gateway);
    }
    let seeds = RESOLVER_SEEDS.get().context("resolver seeds not set")?;
    let conn = connect_to_any_seed(seeds).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&ClientRequest::GetGateways().pack()?).await?;
    send.finish()?;
    let resp = ClientResponse::unpack(&mut recv).await?;
    conn.close(0u32.into(), b"done");
    match resp {
        ClientResponse::GetGateways { gateways } => {
            cache_gateways(&gateways);
            gateways.into_iter().next().context("no gateways registered")
        },
        other => Err(anyhow!("GetGateways: unexpected variant {other:?}")),
    }
}

fn cached_gateway() -> Option<GatewayDescriptor> {
    let conn = NETWORK_DB.lock();
    conn.query_row("SELECT id, addr, pubkey FROM gateways LIMIT 1", [], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?))
    })
    .ok()
    .and_then(|(id, addr, pubkey)| {
        Some(GatewayDescriptor {
            id:     id.parse().ok()?,
            addr:   addr.parse().ok()?,
            pubkey: Bytes(pubkey.try_into().ok()?),
        })
    })
}

fn cache_gateways(gateways: &[GatewayDescriptor]) {
    let conn = NETWORK_DB.lock();
    for g in gateways {
        let _ = conn.execute(
            "INSERT INTO gateways (id, addr, pubkey) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET addr = excluded.addr, pubkey = excluded.pubkey",
            params![g.id.to_string(), g.addr.to_string(), g.pubkey.0.as_slice()],
        );
    }
}

fn evict_gateway(id: &RelayId) {
    let conn = NETWORK_DB.lock();
    let _ = conn.execute("DELETE FROM gateways WHERE id = ?1", params![id.to_string()]);
}
