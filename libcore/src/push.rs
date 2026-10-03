//! Offline-wake registration under a random push pseudonym `P`, unrelated to the IPK: the home
//! relay learns `IPK → P` and a gateway learns `P → token`.

use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

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
use common::proto::push::PushProvider;
use common::proto::push::RegisterResponse;
use common::proto::push::RegisterToken;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use ed25519_dalek::SigningKey;
use ed25519_dalek::ed25519::signature::rand_core::OsRng;
use ed25519_dalek::ed25519::signature::rand_core::RngCore;
use std::sync::LazyLock;
use rusqlite::params;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use x509_parser::der_parser::Oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

use crate::data::identity::IdentitySigner;
use crate::db::all;
use crate::db::one;
use crate::quic::dialer::connect_to_any_seed;
use crate::state::core;

static PUSH_KEY: LazyLock<parking_lot::Mutex<Option<SigningKey>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));
static REGISTRATION: Mutex<()> = Mutex::const_new(());
static REFRESH: Notify = Notify::const_new();
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);
const GATEWAY_RETRY_AFTER: Duration = Duration::from_secs(600);

static REFUSED_GATEWAYS: LazyLock<parking_lot::Mutex<HashMap<RelayId, Instant>>> =
    LazyLock::new(Default::default);
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Per-install key, sealed by the platform and excluded from identity backups.
/// Restarting the app must not replace a still-working relay/gateway binding.
fn push_key() -> Result<SigningKey> {
    let mut key = PUSH_KEY.lock();
    if let Some(key) = key.as_ref() {
        return Ok(key.clone());
    }
    let store = core().secure_store.get().context("secure store not initialized")?;
    let db = core().db.network().lock();
    let loaded = load_push_key(&db, store.as_ref())?;
    *key = Some(loaded.clone());
    Ok(loaded)
}

fn load_push_key(
    db: &rusqlite::Connection, store: &dyn crate::platform::SecureStore,
) -> Result<SigningKey> {
    let sealed: Option<Vec<u8>> =
        one(db, "SELECT sealed_key FROM push_identity WHERE singleton = 1", [], |r| r.get(0))?;
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

pub async fn maintain_registration(cancel: CancellationToken) -> Result<()> {
    let mut wait = Duration::ZERO;
    let mut backoff = Duration::from_secs(5);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {},
            _ = REFRESH.notified() => {},
            _ = cancel.cancelled() => return Ok(()),
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

/// The relay binds `P` to this connection's authenticated IPK and stops the stream to refuse it.
pub async fn register_push() -> Result<()> {
    let pseudonym = push_key()?.verifying_key().to_bytes();
    let timestamp = now_ms();
    let ipk = crate::data::identity::Identity::local_ipk().context("no identity")?;
    let sig = IdentitySigner::sign(&push_pseudonym_signing_input(&ipk, &pseudonym, timestamp))?
        .to_bytes();
    let bytes =
        CRelayPacket::RegisterPush { pseudonym: Bytes(pseudonym), timestamp, sig: Bytes(sig) }
            .pack()
            .map_err(|e| anyhow!("pack register_push: {e}"))?;
    let Some(conn) = core().session().map(|s| s.conn.clone()) else { return Ok(()) };
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

/// The platform push token (FCM), registered with a gateway under `P`.
static PUSH_TOKEN: parking_lot::RwLock<Option<Vec<u8>>> = parking_lot::RwLock::new(None);

pub async fn set_push_token(token: Vec<u8>) -> Result<()> {
    *PUSH_TOKEN.write() = Some(token);
    request_registration();
    register_token_at_gateway().await
}

/// Dials the gateway directly so the relay never learns the token, and signs with `P` so the
/// gateway never learns the IPK.
pub async fn register_token_at_gateway() -> Result<()> {
    let _guard = REGISTRATION.lock().await;
    if core().net.get().is_none() {
        return Err(anyhow!("endpoint not initialized"));
    }
    let Some(token) = PUSH_TOKEN.read().clone() else { return Ok(()) };
    let mut gateways = match timeout(REGISTRATION_TIMEOUT, fetch_gateways()).await {
        Ok(Ok(gateways)) => gateways,
        _ => cached_gateways(),
    };
    // The directory is unverified and ordered by id, like the relay's fanout:
    // walk it until one gateway proves itself, skipping recent refusals.
    gateways.sort_by_key(|gateway| gateway.id);
    let mut error = anyhow!("no push gateways available");
    for gateway in gateways {
        let refused_at = REFUSED_GATEWAYS.lock().get(&gateway.id).copied();
        if refused_at.is_some_and(|at| at.elapsed() < GATEWAY_RETRY_AFTER) {
            continue;
        }
        match timeout(REGISTRATION_TIMEOUT, send_registration(&gateway, token.clone())).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(e)) => error = e,
            Err(e) => error = e.into(),
        }
        REFUSED_GATEWAYS.lock().insert(gateway.id, Instant::now());
    }
    Err(error)
}

/// CA-attested capabilities from a dialed node's leaf cert. The node is the TLS server, so its
/// chain was validated against the root CA in the handshake.
pub(crate) fn capabilities_from_conn(conn: &quinn::Connection) -> Option<NodeCapabilities> {
    let identity = conn.peer_identity()?;
    let chain = identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()?;
    let (_, cert) = X509Certificate::from_der(chain.first()?.as_ref()).ok()?;
    let oid = Oid::from(CAPABILITY_OID).ok()?;
    let ext = cert.extensions().iter().find(|e| e.oid == oid)?;
    NodeCapabilities::decode(ext.value)
}

async fn send_registration(gateway: &GatewayDescriptor, token: Vec<u8>) -> Result<()> {
    let reg = RegisterToken::signed(&push_key()?, PushProvider::Fcm, token);
    let conn = crate::quic::dialer::connect(gateway.addr, &gateway.id.to_string()).await?;

    // The resolver's gateway directory is unauthenticated, so the token only goes to a node whose
    // CA-issued cert carries PUSH_GATEWAY.
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
    let seeds = &core().net.get().context("resolver seeds not set")?.seeds;
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
    let rows = all(&core().db.network().lock(), "SELECT id, addr, pubkey FROM gateways", [], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, [u8; 32]>(2)?))
    });
    rows.unwrap_or_default()
        .into_iter()
        .filter_map(|(id, addr, pubkey)| {
            Some(GatewayDescriptor {
                id: id.parse().ok()?,
                addr: addr.parse().ok()?,
                pubkey: Bytes(pubkey),
            })
        })
        .collect()
}

fn cache_gateways(gateways: &[GatewayDescriptor]) {
    let conn = core().db.network().lock();
    for g in gateways {
        let _ = conn.execute(
            "INSERT INTO gateways (id, addr, pubkey) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET addr = excluded.addr, pubkey = excluded.pubkey",
            params![g.id.to_string(), g.addr.to_string(), g.pubkey.0.as_slice()],
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::CoreError;
    use crate::platform::SecureStore;

    struct XorStore;

    impl SecureStore for XorStore {
        fn seal(&self, bytes: Vec<u8>) -> Result<Vec<u8>, CoreError> {
            Ok(bytes.into_iter().map(|b| b ^ 0xA5).collect())
        }

        fn open(&self, bytes: Vec<u8>) -> Result<Vec<u8>, CoreError> {
            self.seal(bytes)
        }
    }

    /// A new key would orphan the gateway registration, and with it every wake.
    #[test]
    fn push_identity_survives_cache_loss() {
        let db = crate::test_support::data::open(crate::db::network::migrate);
        let first = load_push_key(&db, &XorStore).unwrap();
        let reopened = load_push_key(&db, &XorStore).unwrap();
        assert_eq!(first.verifying_key(), reopened.verifying_key());
        let sealed: Vec<u8> =
            db.query_row("SELECT sealed_key FROM push_identity", [], |r| r.get(0)).unwrap();
        assert_ne!(sealed, first.to_bytes(), "the key is stored sealed, never raw");
    }
}
