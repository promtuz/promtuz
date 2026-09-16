//! Offline-wake registration. Mints a push-pseudonym `P` (a random Ed25519
//! keypair, unrelated to the IPK) and tells the home relay `IPK → P`, so the
//! relay can wake this device when a message queues while we're offline. The
//! device token never touches the relay — only the gateway learns it, under
//! `P` (that half is a separate registration).

use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use common::node::capability::CAPABILITY_OID;
use common::node::capability::NodeCapabilities;
use common::proto::RelayId;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_res::ClientRequest;
use common::proto::client_res::ClientResponse;
use common::proto::client_res::GatewayDescriptor;
use common::proto::dht_p2p::push_pseudonym_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::push::GatewayRequest;
use common::proto::push::MAX_PUSH_GATEWAYS;
use common::proto::push::PushProvider;
use common::proto::push::RegisterResponse;
use common::proto::push::RegisterToken;
use common::types::bytes::Bytes;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use once_cell::sync::Lazy;
use rusqlite::OptionalExtension;
use rusqlite::params;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::timeout;
use x509_parser::der_parser::Oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

use crate::ENDPOINT;
use crate::RESOLVER_SEEDS;
use crate::data::identity::IdentitySigner;
use crate::db::network::NETWORK_DB;
use crate::platform::SECURE_STORE;
use crate::quic::dialer::connect_to_any_seed;
use crate::state::RELAY;

static PUSH_KEY: Lazy<parking_lot::Mutex<Option<SigningKey>>> =
    Lazy::new(|| parking_lot::Mutex::new(None));
static REGISTRATION: Mutex<()> = Mutex::const_new(());
static REFRESH: Notify = Notify::const_new();
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Per-install key, sealed by the platform and excluded from identity backups.
/// Restarting the app must not replace a still-working relay/gateway binding.
fn push_key() -> Result<SigningKey> {
    let mut key = PUSH_KEY.lock();
    if let Some(key) = key.as_ref() {
        return Ok(key.clone());
    }
    let store = SECURE_STORE.get().context("secure store not initialized")?;
    let db = NETWORK_DB.lock();
    let loaded = load_push_key(&db, store.as_ref())?;
    *key = Some(loaded.clone());
    Ok(loaded)
}

fn load_push_key(
    db: &rusqlite::Connection, store: &dyn crate::platform::SecureStore,
) -> Result<SigningKey> {
    let sealed: Option<Vec<u8>> = db
        .query_row("SELECT sealed_key FROM push_identity WHERE singleton = 1", [], |r| r.get(0))
        .optional()?;
    let seed = match sealed {
        Some(sealed) => {
            store.open(sealed)?.try_into().map_err(|_| anyhow!("invalid push key length"))?
        },
        None => {
            let mut seed = [0; 32];
            OsRng.fill_bytes(&mut seed);
            let sealed = store.seal(seed.to_vec())?;
            db.execute(
                "INSERT INTO push_identity (singleton, sealed_key) VALUES (1, ?1)",
                [sealed],
            )?;
            seed
        },
    };
    Ok(SigningKey::from_bytes(&seed))
}

pub fn request_registration() {
    REFRESH.notify_one();
}

pub async fn maintain_registration() {
    let mut wait = Duration::ZERO;
    let mut backoff = Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {},
            _ = REFRESH.notified() => {},
        }
        let result = async {
            register_push().await?;
            register_token_at_gateway().await
        }
        .await;
        match result {
            Ok(()) => {
                wait = REFRESH_INTERVAL;
                backoff = Duration::from_secs(5);
            },
            Err(e) => {
                log::debug!("PUSH: registration will retry: {e:#}");
                wait = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            },
        }
    }
}

/// Tell the connected home relay our `P`. Fire-and-forget; the relay binds it
/// to the connection-authenticated IPK. Called on each relay connect.
pub async fn register_push() -> Result<()> {
    let pseudonym = push_key()?.verifying_key().to_bytes();
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
    timeout(REGISTRATION_TIMEOUT, async {
        let (mut tx, _rx) = conn.open_bi().await?;
        tx.write_all(&bytes).await?;
        tx.finish()?;
        if tx.stopped().await?.is_some() {
            return Err(anyhow!("relay rejected push registration"));
        }
        Ok(())
    })
    .await??;
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
pub async fn set_push_token(token: Vec<u8>) -> Result<()> {
    *PUSH_TOKEN.write() = Some(token);
    request_registration();
    register_token_at_gateway().await
}

/// Register `P → token` with a discovered gateway, if we hold a token. Dials
/// the gateway *directly* (client/5) so the relay never learns the token, and
/// self-signs with `P` so the gateway never learns the IPK. No-op without a
/// token. Also (re)runs on relay connect.
pub async fn register_token_at_gateway() -> Result<()> {
    let _guard = REGISTRATION.lock().await;
    if ENDPOINT.get().is_none() {
        return Err(anyhow!("endpoint not initialized"));
    }
    let Some(token) = PUSH_TOKEN.read().clone() else { return Ok(()) };
    let mut gateways = match timeout(REGISTRATION_TIMEOUT, fetch_gateways()).await {
        Ok(Ok(gateways)) => gateways,
        _ => cached_gateways(),
    };
    // Match the relay's bounded wake fanout, including in larger networks.
    gateways.sort_by_key(|gateway| gateway.id);
    gateways.truncate(MAX_PUSH_GATEWAYS);
    let mut error = anyhow!("no push gateways available");
    for gateway in gateways {
        match timeout(REGISTRATION_TIMEOUT, send_registration(&gateway, token.clone())).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => error = e,
            Err(e) => error = e.into(),
        }
        evict_gateway(&gateway.id);
    }
    Err(error)
}

/// The CA-attested capabilities in a dialed node's leaf cert, if it carries the
/// extension. The dialed node is the TLS server, so its chain is always
/// present and was validated against the root CA during the handshake.
pub(crate) fn capabilities_from_conn(conn: &quinn::Connection) -> Option<NodeCapabilities> {
    let identity = conn.peer_identity()?;
    let chain = identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()?;
    let (_, cert) = X509Certificate::from_der(chain.first()?.as_ref()).ok()?;
    let oid = Oid::from(CAPABILITY_OID).ok()?;
    let ext = cert.extensions().iter().find(|e| e.oid == oid)?;
    NodeCapabilities::decode(ext.value)
}

async fn send_registration(gateway: &GatewayDescriptor, token: Vec<u8>) -> Result<()> {
    // ponytail: Fcm-only for now (Android). Pass the provider from the app when
    // iOS / UnifiedPush land.
    let reg = RegisterToken::signed(&push_key()?, PushProvider::Fcm, token);
    let endpoint = ENDPOINT.get().context("endpoint not initialized")?;
    let conn = endpoint.connect(gateway.addr, &gateway.id.to_string())?.await?;

    // The resolver's gateway directory is unauthenticated — the device token
    // only goes to a node the CA stamped PUSH_GATEWAY (relay/src/dht/push_wake.rs
    // makes the same check before handing over a pseudonym).
    let caps = capabilities_from_conn(&conn)
        .ok_or_else(|| anyhow!("gateway cert carries no capability extension"))?;
    if !caps.contains(NodeCapabilities::PUSH_GATEWAY) {
        conn.close(0u32.into(), b"not-a-gateway");
        return Err(anyhow!("gateway {} lacks PUSH_GATEWAY", gateway.id));
    }

    let (mut tx, mut rx) = conn.open_bi().await?;
    tx.write_all(&GatewayRequest::Register(reg).pack()?).await?;
    tx.finish()?;
    let response = RegisterResponse::unpack(&mut rx).await?;
    conn.close(0u32.into(), b"registered");
    match response {
        RegisterResponse::Registered => Ok(()),
        RegisterResponse::Rejected => Err(anyhow!("gateway rejected push registration")),
    }
}

pub(crate) async fn fetch_gateways() -> Result<Vec<GatewayDescriptor>> {
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
            Ok(gateways)
        },
        other => Err(anyhow!("GetGateways: unexpected variant {other:?}")),
    }
}

fn cached_gateways() -> Vec<GatewayDescriptor> {
    let conn = NETWORK_DB.lock();
    let Ok(mut stmt) = conn.prepare("SELECT id, addr, pubkey FROM gateways") else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(id, addr, pubkey)| {
            Some(GatewayDescriptor {
                id: id.parse().ok()?,
                addr: addr.parse().ok()?,
                pubkey: Bytes(pubkey.try_into().ok()?),
            })
        })
        .collect()
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

pub(crate) fn evict_gateway(id: &RelayId) {
    let conn = NETWORK_DB.lock();
    let _ = conn.execute("DELETE FROM gateways WHERE id = ?1", params![id.to_string()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::CoreError;
    use crate::platform::SecureStore;

    struct TestStore;
    impl SecureStore for TestStore {
        fn seal(&self, bytes: Vec<u8>) -> Result<Vec<u8>, CoreError> {
            Ok(bytes.into_iter().map(|b| b ^ 0xa5).collect())
        }
        fn open(&self, bytes: Vec<u8>) -> Result<Vec<u8>, CoreError> {
            self.seal(bytes)
        }
    }

    #[test]
    fn push_identity_survives_cache_loss() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE push_identity (singleton INTEGER PRIMARY KEY, sealed_key BLOB NOT NULL)",
        )
        .unwrap();
        let first = load_push_key(&db, &TestStore).unwrap();
        let reopened = load_push_key(&db, &TestStore).unwrap();
        assert_eq!(first.verifying_key(), reopened.verifying_key());
        let sealed: Vec<u8> =
            db.query_row("SELECT sealed_key FROM push_identity", [], |r| r.get(0)).unwrap();
        assert_ne!(sealed, first.to_bytes());
    }
}
