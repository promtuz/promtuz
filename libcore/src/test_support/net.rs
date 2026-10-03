use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::WelcomeEntry;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use quinn::Connection;
use quinn::Endpoint;
use rustls::RootCertStore;
use tempfile::TempDir;
use tokio_util::task::AbortOnDropHandle;

use crate::db::Stores;
use crate::messaging::session::MlsContext;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
use crate::quic::dht_client::DhtClientResult;
use crate::quic::server::Session;
use crate::state::Core;

/// The DHT as one shared map: every party handed the same fake reaches the same homes.
#[derive(Debug, Default)]
pub struct FakeDhtClient {
    /// Each owner's published stash; a fetch takes the oldest record.
    pub published_kps:    Mutex<HashMap<[u8; 32], Vec<KeyPackageRecord>>>,
    /// Queued for `fetch_welcomes` until acked.
    pub welcomes_pending: Mutex<Vec<WelcomeEntry>>,
    /// Returned once by the next fetch instead of its normal outcome.
    pub fetch_error:      Mutex<Option<DhtClientError>>,
}

impl DhtClient for FakeDhtClient {
    async fn publish_keypackages(&self, records: &[KeyPackageRecord]) -> DhtClientResult<()> {
        let mut stashes = self.published_kps.lock();
        for r in records {
            stashes.entry(r.ipk.0).or_default().push(r.clone());
        }
        Ok(())
    }

    async fn fetch_keypackage_for(
        &self, target_ipk: &[u8; 32],
    ) -> DhtClientResult<KeyPackageRecord> {
        if let Some(e) = self.fetch_error.lock().take() {
            return Err(e);
        }
        let mut stashes = self.published_kps.lock();
        let stash = stashes.entry(*target_ipk).or_default();
        if stash.is_empty() {
            return Err(DhtClientError::NoStash);
        }
        Ok(stash.remove(0))
    }

    async fn deliver_welcome(&self, envelope: &WelcomeEnvelopeP) -> DhtClientResult<()> {
        let id: [u8; 8] =
            blake3::hash(&envelope.welcome_blob.0).as_bytes()[..8].try_into().unwrap();
        self.welcomes_pending
            .lock()
            .push(WelcomeEntry { welcome_id: id.into(), envelope: envelope.clone() });
        Ok(())
    }

    async fn fetch_welcomes(&self) -> DhtClientResult<Vec<WelcomeEntry>> {
        Ok(self.welcomes_pending.lock().clone())
    }

    async fn ack_welcomes(&self, welcome_ids: &[[u8; 8]]) -> DhtClientResult<()> {
        self.welcomes_pending.lock().retain(|w| !welcome_ids.contains(&w.welcome_id.0));
        Ok(())
    }
}

/// One device: its identity key and its own in-memory databases.
pub struct Device {
    pub signer:   SigningKey,
    pub ipk:      [u8; 32],
    pub db:       Stores,
    pub provider: PromtuzMlsProvider,
    pub stash:    KeyPackageStash,
    pub buffer:   EpochCatchupBuffer,
    _files:       TempDir,
}

impl Device {
    pub fn new(seed: u8) -> Self {
        let signer = SigningKey::from_bytes(&[seed; 32]);
        let files = tempfile::tempdir().unwrap();
        let db = Stores::in_memory(files.path().to_string_lossy().into_owned());
        Self {
            ipk: signer.verifying_key().to_bytes(),
            signer,
            provider: PromtuzMlsProvider::new(db.mls()),
            stash: KeyPackageStash::new(db.mls()),
            buffer: EpochCatchupBuffer::new(db.mls()),
            db,
            _files: files,
        }
    }

    pub fn ctx<'a, C: DhtClient>(&'a self, dht: &'a C) -> MlsContext<'a, C> {
        MlsContext { provider: &self.provider, stash: &self.stash, buffer: &self.buffer, dht }
    }

    pub async fn publish_keypackage(&self, dht: &FakeDhtClient) {
        let record = self.stash.generate_one(&self.provider, &self.signer).unwrap();
        dht.publish_keypackages(&[record]).await.unwrap();
    }
}

/// `alice` starts a pair with `bob` the way a first send does, and `bob` joins from the Welcome.
pub async fn pair(
    alice: &Device, bob: &Device, dht: &FakeDhtClient,
) -> (MlsGroupHandle, MlsGroupHandle) {
    bob.publish_keypackage(dht).await;
    let group =
        crate::messaging::session::lazy_create_group(&alice.ctx(dht), &alice.ipk, &alice.signer, &bob.ipk)
            .await
            .unwrap();
    let welcome = dht.welcomes_pending.lock().pop().unwrap();
    let joined = crate::mls::process_welcome(&bob.provider, &welcome.envelope).unwrap();
    (group, joined)
}

/// Under a paused clock, time otherwise jumps to the next timer whenever every task waits, even on
/// loopback IO still in flight. Stepping it a millisecond at a time keeps timeouts behind the IO.
pub fn step_paused_clock() -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        loop {
            tick.tick().await;
        }
    }))
}

/// A TLS server config for a fresh "localhost" certificate offering `alpn`, and roots trusting it.
pub fn tls_server(alpn: &[u8]) -> (rustls::ServerConfig, RootCertStore) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key.into())
        .unwrap();
    tls.alpn_protocols = vec![alpn.to_vec()];
    (tls, roots)
}

pub fn quic_server(tls: rustls::ServerConfig) -> quinn::ServerConfig {
    quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
    ))
}

/// A UDP endpoint on 127.0.0.1 serving `role` as "localhost", and the roots that trust it.
pub fn server(role: ProtoRole) -> (Endpoint, RootCertStore) {
    let (tls, roots) = tls_server(role.alpn().as_bytes());
    (Endpoint::server(quic_server(tls), "127.0.0.1:0".parse().unwrap()).unwrap(), roots)
}

/// The production client configuration for `role`, trusting `roots`.
pub fn client_config(role: ProtoRole, roots: &RootCertStore) -> quinn::ClientConfig {
    common::quic::config::build_client_cfg(role, roots).unwrap()
}

pub fn client_endpoint() -> Endpoint {
    Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap()
}

/// Both ends of one loopback connection: ours, then the relay's.
pub async fn connection() -> (Connection, Connection) {
    let (server, roots) = server(ProtoRole::Client);
    let dial = client_endpoint()
        .connect_with(
            client_config(ProtoRole::Client, &roots),
            server.local_addr().unwrap(),
            "localhost",
        )
        .unwrap();
    let (ours, theirs) = tokio::join!(dial, async { server.accept().await.unwrap().await });
    (ours.unwrap(), theirs.unwrap())
}

/// A relay session over `conn`, owned by a throwaway core on the current runtime.
pub fn session(conn: Connection) -> Session {
    let core = Core::new(Stores::in_memory(String::new()), tokio::runtime::Handle::current());
    let relay = crate::data::relay::Relay {
        id:     "relay".into(),
        host:   "127.0.0.1".into(),
        port:   conn.remote_address().port(),
        pubkey: None,
        assist: false,
    };
    let ipk = SigningKey::from_bytes(&[0x5e; 32]).verifying_key();
    Session::new(&core, relay, conn, ipk, None, None)
}
