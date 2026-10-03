use std::net::Ipv4Addr;
use std::sync::Arc;

use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use tempfile::TempDir;

use crate::data::conversation::Conversation;
use crate::data::media::MediaRow;
use crate::data::message::Message;
use crate::p2p::PeerLink;
use crate::quic::peer_config::build_peer_client_cfg;
use crate::quic::peer_config::build_peer_server_cfg;
use crate::quic::peer_config::ipk_binding_message;
use crate::state::Core;
use crate::transfer::store;
use crate::transfer::wire;

/// Every loopback endpoint presents this TLS key, so each test identity binds to it.
const TLS_SEED: [u8; 32] = [0xf4; 32];

pub(crate) struct Device {
    pub core: &'static Core,
    pub dir:  TempDir,
}

/// Leaked to `'static` like the process's core. Spawns run on the calling test's runtime, so a
/// paused clock covers them.
pub(crate) fn device() -> Device {
    let (db, dir) = crate::test_support::data::stores();
    let core = Box::leak(Box::new(Core::new(db, tokio::runtime::Handle::current())));
    Device { core, dir }
}

impl Device {
    pub fn pair(&self, ipk: [u8; 32]) {
        let db = self.core.db.contacts().lock();
        crate::data::contact::Contact::save_pending_tx(&db, ipk, "peer", 0).unwrap();
        crate::data::contact::Contact::mark_paired_tx(&db, &ipk).unwrap();
    }

    pub fn group(&self, members: &[[u8; 32]]) -> [u8; 16] {
        Conversation::join_group_tx(&self.core.db.messages().lock(), &members[0], members).unwrap()
    }

    pub fn retain(&self, bytes: &[u8], chunk: usize) -> wire::Manifest {
        let manifest = manifest(bytes, chunk);
        let path = self.dir.path().join(hex::encode(manifest.file_id()));
        std::fs::write(&path, bytes).unwrap();
        store::retention_put_tx(
            &self.core.db.transfers().lock(),
            &manifest.file_id(),
            path.to_str().unwrap(),
            manifest.total_size,
            manifest.chunk_size,
            &postcard::to_allocvec(&manifest).unwrap(),
            u64::MAX,
        )
        .unwrap();
        manifest
    }

    /// Our outgoing attachment message in `conversation`, which scopes who may pull the file.
    pub fn offer(&self, me: [u8; 32], conversation: [u8; 16], manifest: &wire::Manifest) {
        let db = self.core.db.messages().lock();
        let sent = Message::save_outgoing_tx(&db, conversation, "", None, Some(me)).unwrap();
        let dispatch: [u8; 16] = sent.inner.dispatch_id.unwrap().try_into().unwrap();
        let row = attachment(manifest.file_id(), manifest.total_size);
        crate::data::media::save_tx(&db, &conversation, &dispatch, &row).unwrap();
    }

    pub fn partial(&self, file_id: &[u8; 32]) -> Option<store::Partial> {
        store::partial_get_tx(&self.core.db.transfers().lock(), file_id)
    }

    pub fn verified(&self, file_id: &[u8; 32]) -> u32 {
        let db = self.core.db.transfers().lock();
        store::partial_get_tx(&db, file_id).map_or(0, |p| store::verified_count_tx(&db, &p))
    }
}

pub(crate) fn attachment(file_id: [u8; 32], size: u64) -> MediaRow {
    MediaRow {
        kind: crate::data::media::KIND_ATTACHMENT,
        group_id: None,
        mime: "application/octet-stream".into(),
        name: "f.bin".into(),
        size,
        width: 0,
        height: 0,
        duration_ms: 0,
        blob: None,
        thumb: None,
        file_id: Some(file_id.to_vec()),
        sticker: None,
    }
}

pub(crate) fn manifest(bytes: &[u8], chunk: usize) -> wire::Manifest {
    wire::Manifest {
        total_size: bytes.len() as u64,
        chunk_size: chunk as u32,
        chunks:     bytes.chunks(chunk).map(|c| *blake3::hash(c).as_bytes()).collect(),
    }
}

/// `chunks` pieces of 1 KiB, each a different byte and the last one short.
pub(crate) fn content(seed: u8, chunks: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..chunks {
        bytes.extend(vec![seed.wrapping_add(i as u8); if i + 1 == chunks { 517 } else { 1024 }]);
    }
    bytes
}

pub(crate) fn identity(id: u8) -> wire::Auth {
    let key = SigningKey::from_bytes(&[id; 32]);
    let tls_pub = SigningKey::from_bytes(&TLS_SEED).verifying_key().to_bytes();
    wire::Auth {
        ipk: key.verifying_key().to_bytes(),
        tls_pub,
        sig: key.sign(&ipk_binding_message(&tls_pub)).to_bytes(),
    }
}

pub(crate) struct Handshake {
    pub accepted:  Result<quinn::Connection, quinn::ConnectionError>,
    pub dialed:    Result<quinn::Connection, quinn::ConnectionError>,
    pub endpoints: [quinn::Endpoint; 2],
}

/// The dialer's small receive windows make a large response wait on its reader.
pub(crate) async fn handshake() -> Handshake {
    let _ = common::quic::config::setup_crypto_provider();
    let key = Arc::new(common::quic::config::build_self_signed_ed25519_cert(
        SigningKey::from_bytes(&TLS_SEED),
    ));
    let server = build_peer_server_cfg(key.clone()).unwrap();
    let mut client = build_peer_client_cfg(key).unwrap();
    let mut transport = quinn::TransportConfig::default();
    transport.stream_receive_window((64u32 * 1024).into());
    transport.receive_window((128u32 * 1024).into());
    client.transport_config(Arc::new(transport));
    let server = quinn::Endpoint::server(server, (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    let mut dialer = quinn::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    dialer.set_default_client_config(client);
    let dial = dialer.connect(server.local_addr().unwrap(), "peer").unwrap();
    let (accepted, dialed) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(async { server.accept().await.unwrap().accept().unwrap().await }, dial)
    })
    .await
    .expect("loopback handshake is bounded");
    Handshake { accepted, dialed, endpoints: [server, dialer] }
}

/// The serving end (expecting `receiver`) and the pulling end (expecting `sender`) of one link.
pub(crate) struct Linked {
    pub server:     PeerLink,
    pub client:     PeerLink,
    pub _endpoints: [quinn::Endpoint; 2],
}

pub(crate) async fn linked(sender: &wire::Auth, receiver: &wire::Auth) -> Linked {
    let h = handshake().await;
    Linked {
        server:     crate::p2p::test_link(h.accepted.unwrap(), receiver.ipk),
        client:     crate::p2p::test_link(h.dialed.unwrap(), sender.ipk),
        _endpoints: h.endpoints,
    }
}

/// `n` consecutive 20 ms frames of a 440 Hz tone, encoded as a call sends them.
pub(crate) fn tone_packets(n: usize) -> Vec<Vec<u8>> {
    use crate::call::audio::AudioPath;
    use crate::call::audio::FRAME_SAMPLES;
    use crate::call::audio::SAMPLE_RATE;
    let path = AudioPath::new().unwrap();
    let mut phase = 0f32;
    (0..n)
        .map(|_| {
            let pcm: Vec<u8> = (0..FRAME_SAMPLES)
                .flat_map(|_| {
                    phase += 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
                    ((phase.sin() * 8000.0) as i16).to_le_bytes()
                })
                .collect();
            path.encode(&pcm).unwrap()
        })
        .collect()
}
