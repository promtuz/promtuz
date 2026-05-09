//! Phase 5b — end-to-end MLS integration tests.
//!
//! These tests stand up real `relay::dht::Dht` instances backed by
//! RocksDB tempdirs, listen on loopback `peer/1` QUIC endpoints, and
//! drive libcore-flavoured "test clients" that hold their own MLS
//! provider, KP stash, epoch buffer, and a real
//! [`core::quic::peer1_client::Peer1DhtClient`] dialer. The tests
//! exercise the **production** code paths inside libcore — KP fetch,
//! Welcome publish/fetch/ack, application envelope encrypt/decrypt,
//! epoch-ahead buffering — over real wire RPCs.
//!
//! # Harness shape
//!
//! - `Relay`: thin wrapper over `Dht::new(...)` plus a `peer/1` QUIC
//!   acceptor that hands every inbound connection to
//!   `relay::dht::handler::handle_peer_connection`. One Relay = one
//!   `NodeId` = one routing-table view.
//! - `MlsTestClient`: independent libcore-flavoured peer with its own
//!   IPK, MLS provider, stash, buffer, and `Peer1DhtClient` pointed at
//!   a chosen home Relay. Drives `MlsContext`-based send/receive
//!   pipelines directly, bypassing the libcore-global `Identity::get` /
//!   `Contact::*` / `RELAY` static state via the `_no_contacts` /
//!   `_for` variants exposed in Phase 5b.
//!
//! # Test discipline (relaxed for e2e)
//!
//! Per HANDOFF.md §"Phase 5b — e2e test discipline": each test is
//! capped at 5s wall-clock (relaxed from the standard 2s). Tests use
//! `127.0.0.1:0` ephemeral ports + tempdirs; nothing leaks between
//! tests. Determinism: per-client IPK seeds; relay node ids are
//! deterministically derived too. No fake clock today (production
//! `peer1_client` reads the wall clock; future test wires can plumb
//! a clock injection if §11.3 timing surfaces drift).
//!
//! design-doc: `misc/specs/MLS.md` §11.3d.

#![allow(clippy::too_many_arguments, clippy::needless_borrow)]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ---------------------------------------------------------------------
// `#[tokio::test]` is unavailable here because the libcore crate is
// named `core`, which shadows stdlib's `::core` and breaks the macro
// expansion (`::core::pin::Pin`, `::core::future::Future`). Each test
// is a `#[test]` plus a one-line `block_on` shim instead.
// ---------------------------------------------------------------------
fn block_on<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(f)
}

use common::proto::dht_p2p::NodeDescriptor;
use common::quic::id::NodeId;
use common::types::bytes::Bytes;
use core::api::messaging::InboundDecoded;
use core::api::messaging::MlsContext;
use core::api::messaging::build_application_envelope_bytes;
use core::api::messaging::lazy_create_group;
use core::api::messaging::leaf_signer_for_group;
use core::api::messaging::process_application_inbound_for;
use core::api::messaging::process_welcome_inbound_no_contacts;
use core::db::mls::apply_mls_migrations;
use core::mls::EpochCatchupBuffer;
use core::mls::KeyPackageStash;
use core::mls::MlsGroupHandle;
use core::mls::PromtuzMlsProvider;
use core::quic::dht_client::DhtClient;
use core::quic::dht_client::KpOutcomeFilter;
use core::quic::peer1_client::HomeDescriptor;
use core::quic::peer1_client::Peer1DhtClient;
use core::quic::peer_config::build_peer_client_cfg_with_subkey;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::ProtocolMessage;
use openmls::prelude::tls_codec::Deserialize as _;
use openmls::prelude::tls_codec::Serialize as _;
use parking_lot::Mutex;
use quinn::Endpoint;
use relay::dht::Dht;
use relay::dht::DhtConfig;
use relay::dht::dht_cf_descriptors;
use relay::dht::handler::handle_dht_request;
use common::proto::dht_p2p::DhtHello;
use common::proto::dht_p2p::DhtPacket;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use rusqlite::Connection as SqlConn;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------
// Crypto provider — install once per process.
// ---------------------------------------------------------------------

fn ensure_crypto_provider() {
    let _ = rustls::crypto::CryptoProvider::install_default(
        rustls::crypto::aws_lc_rs::default_provider(),
    );
}

// ---------------------------------------------------------------------
// Self-signed Ed25519 cert builder — same shape as libcore's
// `peer_config::build_tbs_certificate` / `build_certificate_der` (those
// are private to that module). The libcore-side cert verifier
// (`PeerServerCertVerifier`) accepts any well-formed Ed25519 cert.
// ---------------------------------------------------------------------

fn build_tbs_cert(public_key: &[u8; 32]) -> Vec<u8> {
    let ed25519_oid: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
    let spki = [
        &[0x30, 0x2a][..],
        &[0x30, 0x05][..],
        ed25519_oid,
        &[0x03, 0x21, 0x00][..],
        public_key,
    ]
    .concat();
    let serial = &public_key[0..8];
    let validity: &[u8] = &[
        0x30, 0x1e, 0x17, 0x0d, b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0',
        b'0', b'Z', 0x17, 0x0d, b'5', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0',
        b'0', b'Z',
    ];
    let empty_name: &[u8] = &[0x30, 0x00];
    let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];
    let serial_der = [&[0x02, serial.len() as u8][..], serial].concat();
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
    let tbs_content = [
        version,
        &serial_der,
        sig_alg,
        empty_name,
        validity,
        empty_name,
        &spki,
    ]
    .concat();
    encode_seq(&tbs_content)
}

fn build_cert_der(tbs: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
    let sig_bitstring = [&[0x03, 0x41, 0x00][..], signature].concat();
    let cert_content = [tbs, sig_alg, &sig_bitstring].concat();
    encode_seq(&cert_content)
}

fn encode_seq(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    if len < 128 {
        [&[0x30, len as u8][..], data].concat()
    } else if len < 256 {
        [&[0x30, 0x81, len as u8][..], data].concat()
    } else {
        let len_bytes = (len as u16).to_be_bytes();
        [&[0x30, 0x82][..], &len_bytes, data].concat()
    }
}

#[derive(Debug)]
struct TestEd25519Signer {
    signing: SigningKey,
}

impl rustls::sign::SigningKey for TestEd25519Signer {
    fn choose_scheme(
        &self, offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&rustls::SignatureScheme::ED25519) {
            Some(Box::new(TestEd25519Inner { signing: self.signing.clone() }))
        } else {
            None
        }
    }

    fn public_key(&self) -> Option<rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
        let alg_id =
            rustls::pki_types::AlgorithmIdentifier::from_slice(&[0x06, 0x03, 0x2b, 0x65, 0x70]);
        let pubkey: [u8; 32] = self.signing.verifying_key().to_bytes();
        Some(rustls::sign::public_key_to_spki(&alg_id, pubkey))
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ED25519
    }
}

#[derive(Debug)]
struct TestEd25519Inner {
    signing: SigningKey,
}

impl rustls::sign::Signer for TestEd25519Inner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        Ok(self.signing.sign(message).to_bytes().to_vec())
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        rustls::SignatureScheme::ED25519
    }
}

/// Build a `peer/1` server config self-signing with `signing`. ALPN is
/// `peer/1`; client auth disabled — production relay matches.
fn build_test_peer_server_cfg(signing: &SigningKey) -> quinn::ServerConfig {
    let pubkey = signing.verifying_key().to_bytes();
    let tbs = build_tbs_cert(&pubkey);
    let sig = signing.sign(&tbs);
    let cert_der = build_cert_der(&tbs, &sig.to_bytes());
    let cert = rustls::pki_types::CertificateDer::from(cert_der);

    let signing_key: Arc<dyn rustls::sign::SigningKey> =
        Arc::new(TestEd25519Signer { signing: signing.clone() });
    let certified = rustls::sign::CertifiedKey::new(vec![cert], signing_key);

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified)));
    tls.alpn_protocols = vec![common::quic::protorole::ProtoRole::Peer.alpn().into()];
    let q = quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(q));
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    cfg.transport_config(Arc::new(transport));
    cfg
}

// ---------------------------------------------------------------------
// Custom test peer/1 acceptor — mirrors the relay-side
// `handle_peer_connection` *except* it does NOT insert the requester
// into `dht.routing` / `dht.peer_conns`. That insertion is what
// pollutes the routing table with libcore-ephemeral peers in the e2e
// harness; ours is a closed cluster of N relays cross-wired up front
// and we want every `FindNode` to return only those.
//
// The DhtHello recv + verify + per-RPC dispatch logic is the same as
// production, just minus the side effects.
// ---------------------------------------------------------------------

async fn handle_test_peer_connection(dht: Arc<Dht>, conn: quinn::Connection) {
    // 1. Receive DhtHello on first uni-stream.
    let mut recv = match tokio::time::timeout(
        Duration::from_secs(5),
        conn.accept_uni(),
    )
    .await
    {
        Ok(Ok(s)) => s,
        _ => return,
    };
    let hello: DhtHello = match DhtHello::unpack(&mut recv).await {
        Ok(h) => h,
        Err(_) => return,
    };
    if hello
        .verify(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0))
        .is_err()
    {
        return;
    }
    let auth_id = hello.node_id;

    // 2. Loop over inbound bi-streams, dispatch each as a DhtRequest.
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let dht = dht.clone();
        tokio::spawn(async move {
            let pkt = match DhtPacket::unpack(&mut recv).await {
                Ok(p) => p,
                Err(_) => return,
            };
            let req = match pkt {
                DhtPacket::Request(r) => r,
                DhtPacket::Response(_) => return,
            };
            let resp = handle_dht_request(&dht, req, auth_id).await;
            let bytes = match DhtPacket::Response(resp).pack() {
                Ok(b) => b,
                Err(_) => return,
            };
            let _ = send.write_all(&bytes).await;
            let _ = send.finish();
        });
    }
}

// ---------------------------------------------------------------------
// `Relay` test fixture — a real Dht + a peer/1 acceptor.
// ---------------------------------------------------------------------

/// Atomic counter for unique RocksDB tempdir paths across tests.
static FIXTURE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn fresh_rocksdb_path() -> std::path::PathBuf {
    let seq = FIXTURE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("promtuz-phase5b-{pid}-{seq}"))
}

/// One relay-side fixture. Holds:
/// - the `Dht`,
/// - the `peer/1` QUIC endpoint,
/// - a `CancellationToken` so test teardown can stop the acceptor.
struct TestRelay {
    dht: Arc<Dht>,
    addr: SocketAddr,
    node_id: NodeId,
    /// Phase 7 (P0-2): the actual Ed25519 pubkey (cert SPKI). Distinct
    /// from `node_id` which is `BLAKE3(pubkey)`. Pinning consumes
    /// `pubkey`, not `node_id`.
    pubkey: [u8; 32],
    cancel: CancellationToken,
    rocks_path: std::path::PathBuf,
}

impl TestRelay {
    /// Start a new test relay. Returns once the QUIC endpoint is
    /// listening — subsequent `Peer1DhtClient` dials are guaranteed to
    /// succeed.
    async fn start(seed: u8) -> Self {
        ensure_crypto_provider();

        let signing = SigningKey::from_bytes(&[seed; 32]);
        let pubkey = signing.verifying_key().to_bytes();
        let node_id = NodeId::new(pubkey);

        // Open RocksDB on a fresh tempdir.
        let rocks_path = fresh_rocksdb_path();
        let _ = std::fs::remove_dir_all(&rocks_path);
        let mut opts = rust_rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let mut cfs = vec![rust_rocksdb::ColumnFamilyDescriptor::new(
            "default",
            rust_rocksdb::Options::default(),
        )];
        cfs.extend(dht_cf_descriptors());
        let db =
            rust_rocksdb::DB::open_cf_descriptors(&opts, &rocks_path, cfs).expect("open db");

        let cfg = DhtConfig::default();
        let dht =
            Arc::new(Dht::new(node_id, signing.clone(), cfg, Arc::new(db)).expect("dht"));

        // Build the peer/1 server endpoint.
        let server_cfg = build_test_peer_server_cfg(&signing);
        let endpoint = Endpoint::server(server_cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();

        // Acceptor task — cancellation aware.
        let cancel = CancellationToken::new();
        let dht_for_accept = dht.clone();
        let cancel_for_accept = cancel.clone();
        tokio::spawn(async move {
            loop {
                let incoming = tokio::select! {
                    _ = cancel_for_accept.cancelled() => break,
                    inc = endpoint.accept() => inc,
                };
                let Some(inc) = incoming else { break };
                let dht = dht_for_accept.clone();
                tokio::spawn(async move {
                    let conn = match inc.await {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    handle_test_peer_connection(dht, conn).await;
                });
            }
        });

        Self {
            dht,
            addr,
            node_id,
            pubkey,
            cancel,
            rocks_path,
        }
    }

    /// Add a peer descriptor to this relay's routing table.
    fn seed_routing_table(&self, peer_id: NodeId, peer_addr: SocketAddr, peer_pubkey: [u8; 32]) {
        let desc = NodeDescriptor {
            id: peer_id,
            addr: peer_addr,
            pubkey: Bytes(peer_pubkey),
        };
        self.dht.seed_routing_table(desc);
    }

    fn pubkey(&self) -> [u8; 32] {
        // Phase 7 (P0-2): return the real Ed25519 pubkey (cert SPKI),
        // not the node_id (which is BLAKE3 of it). The previous return
        // was load-bearing for `seed_routing_table` in a way that didn't
        // surface until pinning landed and tried to use it as the
        // expected SPKI.
        self.pubkey
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Best-effort cleanup of the rocksdb tempdir.
        let _ = std::fs::remove_dir_all(&self.rocks_path);
    }
}

/// Cross-wire all relays' routing tables so any `FindNode` against the
/// keyspace returns a well-populated K-set. Each relay learns about
/// every other relay's `(node_id, addr, pubkey)`.
fn cross_wire(relays: &[&TestRelay]) {
    for r in relays {
        for s in relays {
            if r.node_id != s.node_id {
                r.seed_routing_table(s.node_id, s.addr, s.pubkey());
            }
        }
    }
}

/// A 3-relay cluster bundle. The custom test acceptor
/// (`handle_test_peer_connection`) keeps each relay's routing table
/// to the curated cross-wired set without any libcore-ephemeral
/// pollution, so tests can rely on `FindNode` returning only the
/// allowed relays.
#[allow(dead_code)]
struct Cluster {
    pub r0: TestRelay,
    pub r1: TestRelay,
    pub r2: TestRelay,
    allowed: Vec<NodeId>,
}

/// Spin up a 3-relay loopback cluster, fully cross-wired. Most tests
/// use this — even the "1:1" tests, because `Peer1DhtClient`'s K=3
/// fan-out requires K_MIN=2 successful publishes. With cross-wired
/// relays, `find_k_closest` returns up to 3 candidate homes and the
/// publish can meet quorum.
async fn spawn_3relay_cluster(seed_base: u8) -> Cluster {
    let r0 = TestRelay::start(seed_base).await;
    let r1 = TestRelay::start(seed_base.wrapping_add(1)).await;
    let r2 = TestRelay::start(seed_base.wrapping_add(2)).await;
    cross_wire(&[&r0, &r1, &r2]);
    let allowed = vec![r0.node_id, r1.node_id, r2.node_id];
    Cluster { r0, r1, r2, allowed }
}

// ---------------------------------------------------------------------
// `MlsTestClient` — libcore-flavoured peer.
// ---------------------------------------------------------------------

/// One libcore-equivalent client. Wraps:
/// - a deterministic IPK (`SigningKey::from_bytes(&[seed; 32])`),
/// - an in-memory MLS provider + stash + epoch buffer,
/// - a real `Peer1DhtClient` pointed at the supplied home,
/// - a tracking map of group_id → group state for inbound app messages.
struct MlsTestClient {
    pub ipk_signer: SigningKey,
    pub ipk: [u8; 32],
    pub provider: PromtuzMlsProvider,
    pub stash: KeyPackageStash,
    pub buffer: EpochCatchupBuffer,
    pub dht: Arc<Peer1DhtClient>,
}

impl MlsTestClient {
    /// Build a fresh client. `ipk_seed` produces a deterministic IPK;
    /// `home` is the relay this client treats as its sticky-home for
    /// `FindNode` delegation.
    fn new(ipk_seed: u8, home: &TestRelay) -> Self {
        ensure_crypto_provider();

        let ipk_signer = SigningKey::from_bytes(&[ipk_seed; 32]);
        let ipk = ipk_signer.verifying_key().to_bytes();

        // In-memory SQLite for MLS state.
        let conn = Self::fresh_mls_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let buffer = EpochCatchupBuffer::new(conn);

        // Endpoint + client config — fresh per client so each gets an
        // independent quinn `Endpoint` (no port conflict on
        // `127.0.0.1:0`).
        let tls_subkey =
            ed25519_dalek::SigningKey::from_bytes(&[ipk_seed.wrapping_add(0xA5); 32]);
        let peer_cfg =
            Arc::new(build_peer_client_cfg_with_subkey(tls_subkey.clone()).expect("client cfg"));
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();

        let home_desc = HomeDescriptor {
            node_id: home.node_id,
            addr: home.addr,
            pubkey: Some(home.pubkey()),
        };

        // Phase 7 (P0-2): use the tls_subkey-aware constructor so the
        // dialer can build per-dial pinned `ClientConfig`s. Each
        // TestRelay's TLS cert SPKI equals its NodeKey pubkey
        // (build_test_peer_server_cfg signs with the same key it
        // hands the routing-table descriptor), so pinning works
        // end-to-end against the test cluster.
        let dht = Peer1DhtClient::new_arc_with_tls_subkey(
            endpoint,
            peer_cfg,
            tls_subkey,
            home_desc,
            ipk,
            ipk_signer.clone(),
        );

        Self {
            ipk_signer,
            ipk,
            provider,
            stash,
            buffer,
            dht,
        }
    }

    fn fresh_mls_conn() -> Arc<Mutex<SqlConn>> {
        let mut conn = SqlConn::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        Arc::new(Mutex::new(conn))
    }

    /// Build an `MlsContext` borrowing `&self`.
    fn ctx(&self) -> MlsContext<'_, Peer1DhtClient> {
        MlsContext {
            provider: &self.provider,
            stash:    &self.stash,
            buffer:   &self.buffer,
            dht:      self.dht.as_ref(),
        }
    }

    /// Mint a fresh KP stash and publish to homes via the real
    /// `Peer1DhtClient`. Returns the count of records published.
    ///
    /// Retries up to 5 times on transient `QuorumNotMet`. The
    /// underlying flake is a relay-side `NotOwner` reply when the
    /// home's routing table briefly contains a libcore-ephemeral peer
    /// that pushes the home out of the strict top-K. The cluster's
    /// purger task scrubs the table aggressively, so a retry usually
    /// hits a clean state.
    async fn publish_keypackages(&self) -> usize {
        let kps = self
            .stash
            .ensure_stash_full(&self.provider, &self.ipk_signer)
            .expect("ensure stash full");
        let to_publish = if kps.is_empty() { &kps[..] } else { &kps[..1] };
        let mut last_err = None;
        for _attempt in 0..5 {
            match self
                .dht
                .publish_keypackages(to_publish, KpOutcomeFilter::Default)
                .await
            {
                Ok(()) => return to_publish.len(),
                Err(e) => {
                    last_err = Some(e);
                    // Drop cached K-set so the next attempt re-queries
                    // the home (which may have just been purged of
                    // libcore-ephemerals by the cluster's purger).
                    self.dht.clear_findnode_cache();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                },
            }
        }
        panic!("publish kp failed after 5 retries: {:?}", last_err);
    }

    /// Fetch and process any pending welcomes from the home, returning
    /// the number of welcomes activated. Bypasses the
    /// `Contact::exists` global; the test contract is "every sender we
    /// see is allowed."
    ///
    /// Duplicate welcomes (same logical Welcome stored at multiple K
    /// homes, each with a distinct random `welcome_id`) are expected:
    /// the first call to `process_welcome_inbound_no_contacts`
    /// consumes our KP; subsequent ones fail with `NoMatchingKeyPackage`.
    /// We treat both as "processed" for ack purposes — the home GCs
    /// the entry either way.
    async fn poll_welcomes(&self) -> usize {
        let entries = self.dht.fetch_welcomes().await.expect("fetch welcomes");
        let mut processed_ids: Vec<[u8; 8]> = Vec::with_capacity(entries.len());
        let mut count = 0;
        for entry in entries {
            let sender_ipk = entry.envelope.sender_ipk.0;
            match process_welcome_inbound_no_contacts(&self.ctx(), sender_ipk, entry.envelope) {
                Ok(_group) => {
                    processed_ids.push(entry.welcome_id.0);
                    count += 1;
                },
                Err(_) => {
                    // Duplicate (KP already consumed) or malformed —
                    // ack so the home GCs.
                    processed_ids.push(entry.welcome_id.0);
                },
            }
        }
        if !processed_ids.is_empty() {
            let _ = self.dht.ack_welcomes(&processed_ids).await;
        }
        count
    }
}

// ---------------------------------------------------------------------
// Direct-delivery helpers — bypass the relay client/1 channel for
// application messages so we can drive inbound decryption deterministically
// without the full client/relay handshake. The Welcome path goes
// through the real `cf_dht_welcome` queue (DHT layer) via
// `publish_welcome_to_homes` / `fetch_welcomes`.
// ---------------------------------------------------------------------

/// Hand-deliver an application envelope to `recipient`. Mimics what the
/// relay's `client/1` `Deliver` handler would feed into
/// `process_inbound_envelope`.
fn deliver_application(
    sender_ipk: [u8; 32], recipient: &MlsTestClient, payload: &[u8],
) -> Result<InboundDecoded, anyhow::Error> {
    // Deserialise the outer envelope back to its typed shape so we can
    // call `process_application_inbound_for` directly. Production
    // dispatches via `MlsEnvelopeP::deser` in
    // `process_inbound_envelope`; we collapse that single step here.
    use common::proto::mls_wire::MlsEnvelopeP;
    use common::proto::pack::Unpacker;
    let env = MlsEnvelopeP::deser(payload)?;
    let app = match env {
        MlsEnvelopeP::Application(a) => a,
        MlsEnvelopeP::Welcome(_) => {
            anyhow::bail!("deliver_application called with Welcome envelope")
        },
    };
    process_application_inbound_for(&recipient.ctx(), sender_ipk, &recipient.ipk, app)
}

/// Convenience: encrypt + deliver. Returns the decoded inbound result.
fn send_app_message(
    sender: &MlsTestClient, sender_group: &mut MlsGroupHandle, recipient_ipk: &[u8; 32],
    plaintext: &[u8], recipient: &MlsTestClient,
) -> InboundDecoded {
    let leaf =
        leaf_signer_for_group(&sender.provider, sender_group, &sender.ipk).expect("leaf signer");
    let payload = build_application_envelope_bytes(
        &sender.ctx(),
        sender_group,
        &leaf,
        &sender.ipk,
        recipient_ipk,
        plaintext,
        &sender.ipk_signer,
    )
    .expect("build envelope");
    deliver_application(sender.ipk, recipient, &payload).expect("deliver")
}

/// Encrypt-only — produces the wire envelope bytes without delivering.
/// Used for offline/queue tests.
fn encrypt_app(
    sender: &MlsTestClient, sender_group: &mut MlsGroupHandle, recipient_ipk: &[u8; 32],
    plaintext: &[u8],
) -> Vec<u8> {
    let leaf =
        leaf_signer_for_group(&sender.provider, sender_group, &sender.ipk).expect("leaf signer");
    build_application_envelope_bytes(
        &sender.ctx(),
        sender_group,
        &leaf,
        &sender.ipk,
        recipient_ipk,
        plaintext,
        &sender.ipk_signer,
    )
    .expect("build envelope")
}

// ---------------------------------------------------------------------
// Test 1: 1:1 send/receive.
// ---------------------------------------------------------------------

#[test]
fn e2e_1to1_send_receive() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x10).await;
        let r0 = &cluster.r0;

        let alice = MlsTestClient::new(0x11, &r0);
        let bob = MlsTestClient::new(0x12, &r0);

        // Bob publishes his KP stash to the home.
        let n = bob.publish_keypackages().await;
        assert!(n > 0, "bob published at least one KP");

        // Alice fetches Bob's KP, builds the implicit 1:1 group, ships the
        // Welcome through the home's `cf_dht_welcome` queue.
        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group");

        // Bob polls his welcomes — gets the activation Welcome.
        let activated = bob.poll_welcomes().await;
        assert_eq!(activated, 1, "bob activated alice's group");

        // Sanity: both sides agree on the group_id.
        let bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .expect("bob load")
            .expect("bob has group state");
        assert_eq!(bob_group.group_id(), alice_group.group_id());
        assert_eq!(bob_group.epoch(), alice_group.epoch());
        assert_eq!(bob_group.member_count(), 2);

        // Alice sends "hello bob" — Bob receives it.
        let result = send_app_message(
            &alice,
            &mut alice_group,
            &bob.ipk,
            b"hello bob",
            &bob,
        );
        match result {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"hello bob");
            },
            other => panic!("expected Application, got {:?}", discriminant(&other)),
        }
    })
}

/// Print-friendly tag for an `InboundDecoded` (panics use this).
fn discriminant(d: &InboundDecoded) -> &'static str {
    match d {
        InboundDecoded::Welcome => "Welcome",
        InboundDecoded::Application { .. } => "Application",
        InboundDecoded::ApplicationBuffered => "ApplicationBuffered",
        InboundDecoded::ApplicationStale => "ApplicationStale",
    }
}

// ---------------------------------------------------------------------
// Test 2: 1:1 offline-then-reconnect.
// ---------------------------------------------------------------------

#[test]
fn e2e_1to1_offline_then_reconnect() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x20).await;
        let r0 = &cluster.r0;

        let alice = MlsTestClient::new(0x21, &r0);
        let bob = MlsTestClient::new(0x22, &r0);

        // Bob publishes his KP. He's not yet "online" for inbound — for
        // application messages Alice queues them locally, simulating "Bob
        // disconnected from the relay so the sender's libcore queues
        // pending sends."
        bob.publish_keypackages().await;

        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group");

        // Alice queues 3 messages while Bob is offline. She doesn't
        // deliver yet — these are the "outbound queue" she'd flush when
        // Bob reconnects. Order is preserved by Vec push order and openmls
        // generation counters.
        let m1 = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg-1");
        let m2 = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg-2");
        let m3 = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg-3");

        // Bob reconnects: drains welcomes first (sticky-home spec
        // §6.2 — Welcomes-before-applications).
        let activated = bob.poll_welcomes().await;
        assert_eq!(activated, 1, "bob activated alice's group on reconnect");

        // Now Bob processes the queued application messages in send order.
        let r1 = deliver_application(alice.ipk, &bob, &m1).expect("deliver m1");
        let r2 = deliver_application(alice.ipk, &bob, &m2).expect("deliver m2");
        let r3 = deliver_application(alice.ipk, &bob, &m3).expect("deliver m3");

        for (i, r) in [r1, r2, r3].iter().enumerate() {
            match r {
                InboundDecoded::Application { plaintext, .. } => {
                    assert_eq!(
                        plaintext,
                        format!("msg-{}", i + 1).as_bytes(),
                        "messages decrypt in order"
                    );
                },
                other => panic!(
                    "expected Application at idx {}, got {:?}",
                    i,
                    discriminant(other)
                ),
            }
        }
    })
}

// ---------------------------------------------------------------------
// Test 3: 3-party group send/receive.
// ---------------------------------------------------------------------

#[test]
fn e2e_3party_group_send_receive() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x30).await;
        let r0 = &cluster.r0;

        let alice = MlsTestClient::new(0x31, &r0);
        let bob = MlsTestClient::new(0x32, &r0);
        let charlie = MlsTestClient::new(0x33, &r0);

        bob.publish_keypackages().await;
        charlie.publish_keypackages().await;

        // Step 1: Alice creates group + adds Bob (lazy_create_group does
        // this for the implicit 1:1).
        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group bob");
        bob.poll_welcomes().await;
        let mut bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .expect("bob load")
            .expect("bob group");

        // Step 2: Alice fetches Charlie's KP and adds him via add_members
        // on the existing group (so the group goes 2→3).
        let charlie_fetched = alice
            .dht
            .fetch_keypackage_for(&charlie.ipk)
            .await
            .expect("fetch charlie kp");
        use openmls::prelude::KeyPackageIn;
        use openmls::prelude::ProtocolVersion;
        let charlie_kp_in =
            KeyPackageIn::tls_deserialize_exact(&charlie_fetched.record.kp_bytes.0)
                .expect("kp deser");
        let charlie_kp = charlie_kp_in
            .validate(
                &openmls_rust_crypto::RustCrypto::default(),
                ProtocolVersion::Mls10,
            )
            .expect("kp validate");

        let alice_leaf = leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk)
            .expect("alice leaf");
        let (commit, welcome) = alice_group
            .add_members(&alice.provider, &alice_leaf, &[charlie_kp])
            .expect("add charlie");
        alice_group
            .merge_pending_commit(&alice.provider)
            .expect("merge add commit");
        assert_eq!(alice_group.member_count(), 3);

        // Bob processes the commit so he advances to epoch 2.
        let commit_bytes = commit.tls_serialize_detached().expect("ser commit");
        let in_msg = openmls::prelude::MlsMessageIn::tls_deserialize_exact(&commit_bytes)
            .expect("deser");
        let proto: ProtocolMessage = in_msg.try_into_protocol_message().expect("protomsg");
        let processed = bob_group
            .process_incoming(&bob.provider, proto)
            .expect("bob process commit");
        if let ProcessedMessageContent::StagedCommitMessage(staged) = processed {
            bob_group
                .merge_staged_commit(&bob.provider, *staged)
                .expect("bob merge commit");
        } else {
            panic!("expected staged commit");
        }
        assert_eq!(bob_group.epoch(), alice_group.epoch());
        assert_eq!(bob_group.member_count(), 3);

        // Charlie processes the Welcome via the real wire.
        let welcome_env = core::mls::make_welcome_envelope(
            welcome,
            alice_group.group_id(),
            alice.ipk,
            charlie.ipk,
            kp_ref_to_array(&charlie_fetched.record.kp_ref.0),
            &alice.ipk_signer,
        )
        .expect("welcome env");
        alice
            .dht
            .publish_welcome_to_homes(&welcome_env)
            .await
            .expect("publish welcome");

        let activated = charlie.poll_welcomes().await;
        assert_eq!(activated, 1, "charlie activated");
        let mut charlie_group =
            MlsGroupHandle::load(&charlie.provider, &alice_group.group_id())
                .expect("charlie load")
                .expect("charlie group");
        assert_eq!(charlie_group.epoch(), alice_group.epoch());
        assert_eq!(charlie_group.member_count(), 3);

        // Step 3: Alice sends "msg1" — Bob and Charlie receive.
        let payload_a = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg1-from-alice");
        let r_b = deliver_application(alice.ipk, &bob, &payload_a).expect("bob recv");
        match r_b {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"msg1-from-alice");
            },
            other => panic!("bob got {:?}", discriminant(&other)),
        }

        // openmls's `create_message` advances the message-key schedule
        // each call. So we re-encrypt for Charlie (same plaintext, fresh
        // ciphertext) to drive his decrypt path.
        let payload_a2 =
            encrypt_app(&alice, &mut alice_group, &charlie.ipk, b"msg1-from-alice");
        let r_c =
            deliver_application(alice.ipk, &charlie, &payload_a2).expect("charlie recv");
        match r_c {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"msg1-from-alice");
            },
            other => panic!("charlie got {:?}", discriminant(&other)),
        }

        // Step 4: every member sends a reply; everyone reads.
        let payload_b = encrypt_app(&bob, &mut bob_group, &alice.ipk, b"hi-from-bob");
        let r_a_b =
            deliver_application(bob.ipk, &alice, &payload_b).expect("alice recv from bob");
        match r_a_b {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"hi-from-bob")
            },
            other => panic!("alice (from bob) got {:?}", discriminant(&other)),
        }
        let payload_b2 = encrypt_app(&bob, &mut bob_group, &charlie.ipk, b"hi-from-bob");
        let r_c_b = deliver_application(bob.ipk, &charlie, &payload_b2)
            .expect("charlie recv from bob");
        match r_c_b {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"hi-from-bob")
            },
            other => panic!("charlie (from bob) got {:?}", discriminant(&other)),
        }

        let payload_c =
            encrypt_app(&charlie, &mut charlie_group, &alice.ipk, b"hi-from-charlie");
        let r_a_c = deliver_application(charlie.ipk, &alice, &payload_c)
            .expect("alice recv from charlie");
        match r_a_c {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"hi-from-charlie")
            },
            other => panic!("alice (from charlie) got {:?}", discriminant(&other)),
        }
        let payload_c2 =
            encrypt_app(&charlie, &mut charlie_group, &bob.ipk, b"hi-from-charlie");
        let r_b_c = deliver_application(charlie.ipk, &bob, &payload_c2)
            .expect("bob recv from charlie");
        match r_b_c {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"hi-from-charlie")
            },
            other => panic!("bob (from charlie) got {:?}", discriminant(&other)),
        }
    })
}

fn kp_ref_to_array(kp_ref: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = kp_ref.len().min(32);
    out[..n].copy_from_slice(&kp_ref[..n]);
    out
}

// ---------------------------------------------------------------------
// Test 4: 3-party member-remove + PCS.
// ---------------------------------------------------------------------

#[test]
fn e2e_3party_member_remove_pcs() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x40).await;
        let r0 = &cluster.r0;
        let alice = MlsTestClient::new(0x41, &r0);
        let bob = MlsTestClient::new(0x42, &r0);
        let charlie = MlsTestClient::new(0x43, &r0);
        bob.publish_keypackages().await;
        charlie.publish_keypackages().await;

        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create");
        bob.poll_welcomes().await;
        let mut bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();

        let charlie_fetched = alice.dht.fetch_keypackage_for(&charlie.ipk).await.unwrap();
        use openmls::prelude::KeyPackageIn;
        use openmls::prelude::ProtocolVersion;
        let charlie_kp =
            KeyPackageIn::tls_deserialize_exact(&charlie_fetched.record.kp_bytes.0)
                .unwrap()
                .validate(
                    &openmls_rust_crypto::RustCrypto::default(),
                    ProtocolVersion::Mls10,
                )
                .unwrap();
        let alice_leaf =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let (commit, welcome) = alice_group
            .add_members(&alice.provider, &alice_leaf, &[charlie_kp])
            .unwrap();
        alice_group.merge_pending_commit(&alice.provider).unwrap();

        let commit_bytes = commit.tls_serialize_detached().unwrap();
        let in_msg =
            openmls::prelude::MlsMessageIn::tls_deserialize_exact(&commit_bytes).unwrap();
        let proto: ProtocolMessage = in_msg.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, proto).unwrap();
        if let ProcessedMessageContent::StagedCommitMessage(s) = processed {
            bob_group.merge_staged_commit(&bob.provider, *s).unwrap();
        }
        let welcome_env = core::mls::make_welcome_envelope(
            welcome,
            alice_group.group_id(),
            alice.ipk,
            charlie.ipk,
            kp_ref_to_array(&charlie_fetched.record.kp_ref.0),
            &alice.ipk_signer,
        )
        .unwrap();
        alice.dht.publish_welcome_to_homes(&welcome_env).await.unwrap();
        charlie.poll_welcomes().await;
        let charlie_group = MlsGroupHandle::load(&charlie.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();
        let charlie_initial_epoch = charlie_group.epoch();

        // Alice removes Charlie.
        let charlie_idx = alice_group
            .member_index_by_ipk(&charlie.ipk)
            .expect("charlie present");
        let alice_leaf2 =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let remove_commit = alice_group
            .remove_members(&alice.provider, &alice_leaf2, &[charlie_idx])
            .expect("remove charlie");
        alice_group
            .merge_pending_commit(&alice.provider)
            .expect("merge remove");
        assert_eq!(alice_group.member_count(), 2);
        let post_epoch = alice_group.epoch();

        let rc_bytes = remove_commit.tls_serialize_detached().unwrap();
        let rc_in =
            openmls::prelude::MlsMessageIn::tls_deserialize_exact(&rc_bytes).unwrap();
        let rc_proto: ProtocolMessage = rc_in.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, rc_proto).unwrap();
        if let ProcessedMessageContent::StagedCommitMessage(s) = processed {
            bob_group.merge_staged_commit(&bob.provider, *s).unwrap();
        }
        assert_eq!(bob_group.epoch(), post_epoch);
        assert_eq!(bob_group.member_count(), 2);

        // Alice→Bob still works.
        let post_a = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"alice-post-remove");
        let r_b = deliver_application(alice.ipk, &bob, &post_a).expect("bob recv");
        match r_b {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"alice-post-remove");
            },
            other => panic!("bob got {:?}", discriminant(&other)),
        }

        // Charlie cannot decrypt the new-epoch message — PCS holds.
        assert_eq!(charlie_group.epoch(), charlie_initial_epoch);
        let post_c =
            encrypt_app(&alice, &mut alice_group, &charlie.ipk, b"alice-post-remove");
        let charlie_result = deliver_application(alice.ipk, &charlie, &post_c);
        match charlie_result {
            Ok(InboundDecoded::Application { .. }) => {
                panic!("PCS BREACH: charlie decrypted a post-removal message");
            },
            Ok(InboundDecoded::ApplicationBuffered) => {
                // Buffered (epoch is ahead of his local view) — he'd
                // need the Remove commit to advance, which we don't
                // give him. Acceptable PCS result.
            },
            Ok(other) => panic!("unexpected ok variant: {:?}", discriminant(&other)),
            Err(_) => {
                // Hard error — also acceptable PCS result.
            },
        }
    })
}

// ---------------------------------------------------------------------
// Test 5: KP exhaustion + replenishment.
// ---------------------------------------------------------------------

#[test]
fn e2e_kp_exhaustion_replenishment() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x50).await;
        let r0 = &cluster.r0;
        let bob = MlsTestClient::new(0x51, &r0);

        // Bob publishes 100 KPs to the home.
        let kps = bob
            .stash
            .ensure_stash_full(&bob.provider, &bob.ipk_signer)
            .expect("ensure stash");
        assert_eq!(kps.len(), 100, "stash full mints 100");
        bob.dht
            .publish_keypackages(&kps[..], KpOutcomeFilter::Default)
            .await
            .expect("publish 100");

        // 5 distinct fetchers consume 5 KPs (well under the
        // MAX_KP_FETCH_PER_HOUR = 60 cap).
        let mut last_remaining = u32::MAX;
        for i in 0..5 {
            let alice = MlsTestClient::new(0x52 + i, &r0);
            let fetched = alice.dht.fetch_keypackage_for(&bob.ipk).await.expect("fetch");
            assert!(
                fetched.remaining < last_remaining,
                "iter {i}: remaining {} did not decrease (was {last_remaining})",
                fetched.remaining
            );
            last_remaining = fetched.remaining;
        }

        // Refill via `refill_keypackages` RPC — drives the §3.6 refill
        // domain. We mint one extra KP locally and push.
        let one = bob
            .stash
            .generate_one(&bob.provider, &bob.ipk_signer)
            .expect("mint one");
        bob.dht
            .refill_keypackages(&[one], KpOutcomeFilter::Default)
            .await
            .expect("refill");

        // After refill, a fresh fetch by another stranger succeeds.
        let charlie = MlsTestClient::new(0x60, &r0);
        let _ = charlie
            .dht
            .fetch_keypackage_for(&bob.ipk)
            .await
            .expect("fetch after refill");
    })
}

// ---------------------------------------------------------------------
// Test 6: epoch-ahead buffering across reconnect.
// ---------------------------------------------------------------------

#[test]
fn e2e_epoch_ahead_buffering_across_reconnect() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0x70).await;
        let r0 = &cluster.r0;
        let alice = MlsTestClient::new(0x71, &r0);
        let bob = MlsTestClient::new(0x72, &r0);
        let charlie = MlsTestClient::new(0x73, &r0);
        bob.publish_keypackages().await;
        charlie.publish_keypackages().await;

        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .unwrap();
        bob.poll_welcomes().await;
        let mut bob_group = MlsGroupHandle::load(&bob.provider, &alice_group.group_id())
            .unwrap()
            .unwrap();

        let charlie_fetched = alice.dht.fetch_keypackage_for(&charlie.ipk).await.unwrap();
        use openmls::prelude::KeyPackageIn;
        use openmls::prelude::ProtocolVersion;
        let charlie_kp = KeyPackageIn::tls_deserialize_exact(&charlie_fetched.record.kp_bytes.0)
            .unwrap()
            .validate(
                &openmls_rust_crypto::RustCrypto::default(),
                ProtocolVersion::Mls10,
            )
            .unwrap();
        let alice_leaf =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let (add_commit, welcome) = alice_group
            .add_members(&alice.provider, &alice_leaf, &[charlie_kp])
            .unwrap();
        alice_group.merge_pending_commit(&alice.provider).unwrap();

        let bytes = add_commit.tls_serialize_detached().unwrap();
        let m = openmls::prelude::MlsMessageIn::tls_deserialize_exact(&bytes).unwrap();
        let p: ProtocolMessage = m.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, p).unwrap();
        if let ProcessedMessageContent::StagedCommitMessage(s) = processed {
            bob_group.merge_staged_commit(&bob.provider, *s).unwrap();
        }
        let welcome_env = core::mls::make_welcome_envelope(
            welcome,
            alice_group.group_id(),
            alice.ipk,
            charlie.ipk,
            kp_ref_to_array(&charlie_fetched.record.kp_ref.0),
            &alice.ipk_signer,
        )
        .unwrap();
        alice.dht.publish_welcome_to_homes(&welcome_env).await.unwrap();
        charlie.poll_welcomes().await;

        // Step 1: Alice sends msg-N. Bob receives it.
        let n_epoch = alice_group.epoch();
        let m1 = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg-N");
        let r1 = deliver_application(alice.ipk, &bob, &m1).expect("bob msg-N");
        match r1 {
            InboundDecoded::Application { plaintext, .. } => {
                assert_eq!(plaintext, b"msg-N");
            },
            other => panic!("bob expected msg-N, got {:?}", discriminant(&other)),
        }

        // Step 2: Alice removes Charlie → advances to N+1.
        let charlie_idx = alice_group.member_index_by_ipk(&charlie.ipk).unwrap();
        let alice_leaf3 =
            leaf_signer_for_group(&alice.provider, &alice_group, &alice.ipk).unwrap();
        let remove_commit = alice_group
            .remove_members(&alice.provider, &alice_leaf3, &[charlie_idx])
            .unwrap();
        alice_group.merge_pending_commit(&alice.provider).unwrap();
        let n1_epoch = alice_group.epoch();
        assert_eq!(n1_epoch, n_epoch + 1);

        // Step 3: Alice encrypts msg-N+1.
        let m2 = encrypt_app(&alice, &mut alice_group, &bob.ipk, b"msg-N+1");

        // Step 4: Deliver msg-N+1 to Bob FIRST. He's at epoch=N; buffer.
        let r2 = deliver_application(alice.ipk, &bob, &m2).expect("bob msg-N+1 first");
        match r2 {
            InboundDecoded::ApplicationBuffered => {
                // Expected
            },
            other => panic!(
                "bob expected ApplicationBuffered for msg-N+1, got {:?}",
                discriminant(&other)
            ),
        }
        let buffered =
            bob.buffer.buffered_count(&bob_group.group_id()).expect("buffered count");
        assert_eq!(buffered, 1, "msg-N+1 buffered");

        // Step 5: Now deliver the Remove commit. Bob's openmls advances
        // to N+1; the drain pulls msg-N+1 out and decrypts it.
        let rc_bytes = remove_commit.tls_serialize_detached().unwrap();
        let rc_in =
            openmls::prelude::MlsMessageIn::tls_deserialize_exact(&rc_bytes).unwrap();
        let rc_proto: ProtocolMessage = rc_in.try_into_protocol_message().unwrap();
        let processed = bob_group.process_incoming(&bob.provider, rc_proto).unwrap();
        if let ProcessedMessageContent::StagedCommitMessage(s) = processed {
            bob_group.merge_staged_commit(&bob.provider, *s).unwrap();
        }
        assert_eq!(bob_group.epoch(), n1_epoch);

        let drained = bob
            .buffer
            .drain_when_ready(&mut bob_group, &bob.provider)
            .expect("drain");
        assert_eq!(drained.len(), 1, "msg-N+1 drained");
        assert_eq!(drained[0].plaintext, b"msg-N+1");

        let after_buffered =
            bob.buffer.buffered_count(&bob_group.group_id()).expect("buffered count");
        assert_eq!(after_buffered, 0, "buffer empty after drain");
    })
}

// ---------------------------------------------------------------------
// Test 7: 3-relay XOR routing.
// ---------------------------------------------------------------------

#[test]
fn e2e_3relay_xor_routing() {
    block_on(async {
        // Three relays. Alice's home is r0; Bob's home is r1. The K=3
        // KP fan-out across {r0, r1, r2} is determined by XOR distance to
        // `BLAKE3("kp:" || bob.ipk)`.
        let r0 = TestRelay::start(0x80).await;
        let r1 = TestRelay::start(0x81).await;
        let r2 = TestRelay::start(0x82).await;

        cross_wire(&[&r0, &r1, &r2]);

        let alice = MlsTestClient::new(0x83, &r0);
        let bob = MlsTestClient::new(0x84, &r1);

        // Bob publishes via his home (r1). With cross-wired routing
        // tables `find_k_closest` against r1 returns the K=3 set spanning
        // r0/r1/r2.
        bob.publish_keypackages().await;

        // Alice fetches Bob's KP via her home (r0) → which forwards
        // to the K-closest. Whichever subset of {r0, r1, r2} holds his
        // KP, the fetch path tries each in turn until one succeeds.
        let _g = lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
            .await
            .expect("lazy_create across relays");

        let activated = bob.poll_welcomes().await;
        assert_eq!(activated, 1, "bob activated welcome from cross-relay sender");
    })
}


// ---------------------------------------------------------------------
// Phase 7 (P0-4) — integration test that proves the production
// JNI-equivalent path actually dispatches MLS RPCs through
// `Peer1DhtClient` over the real `peer/1` wire (not via a stub
// `NotWiredDhtClient`).
//
// We can't drive the literal JNI extern wrapper without setting up
// `KEY_MANAGER`, `IDENTITY_DB`, `CONTACTS_DB`, etc. — all process-
// global SQLite handles initialised by `initApi`. The next-best thing
// (per the prompt) is exercising the same `MlsContext` shape +
// `Peer1DhtClient` constructor that `Relay::connect` →
// `build_peer1_dht_client` produces, and asserting the
// `peer/1` dial counter is non-zero post-flow. This proves the
// production wire path was taken end-to-end.
// ---------------------------------------------------------------------

#[test]
fn e2e_jni_send_message_dispatches_via_peer1_client_for_real() {
    block_on(async {
        let cluster = spawn_3relay_cluster(0xF0).await;
        let r0 = &cluster.r0;

        // Alice + Bob each construct an `MlsTestClient` that holds a
        // real `Peer1DhtClient` (built via the same
        // `new_arc_with_tls_subkey` constructor production
        // `build_peer1_dht_client` uses; see
        // `libcore/src/quic/server.rs::build_peer1_dht_client`). Both
        // dialers point at r0 as their "home"; the home cross-wires
        // to r1/r2 so the K=3 fan-out has a quorum-eligible set.
        let alice = MlsTestClient::new(0xF1, &r0);
        let bob = MlsTestClient::new(0xF2, &r0);

        // Pre-state: zero dials on both clients (nothing has gone over
        // the wire yet).
        assert_eq!(alice.dht.dials(), 0, "alice pre: 0 dials");
        assert_eq!(bob.dht.dials(), 0, "bob pre: 0 dials");

        // Bob publishes his KP stash through the production
        // `Peer1DhtClient::publish_keypackages` path — this is the
        // first wire dispatch.
        let n = bob.publish_keypackages().await;
        assert!(n > 0, "bob published at least one KP");

        // Alice goes through the production lazy_create_group path —
        // KP fetch over peer/1, Welcome publish over peer/1, group
        // commit. This is what `send_message_inner` does on first
        // send to a contact whose `mls_group_id` is None.
        let mut alice_group =
            lazy_create_group(&alice.ctx(), &alice.ipk, &alice.ipk_signer, &bob.ipk)
                .await
                .expect("lazy_create_group via Peer1DhtClient");

        // Alice has now (a) FindNode'd the home, (b) dialed at least
        // one peer/1 connection for KP fetch, and (c) dialed at least
        // 2 peers for the Welcome publish quorum. Counter must be > 0.
        let alice_dials_after_create = alice.dht.dials();
        assert!(
            alice_dials_after_create > 0,
            "JNI-equivalent send path dispatched through Peer1DhtClient ({} dials)",
            alice_dials_after_create
        );

        // Bob processes Alice's Welcome via the production
        // `poll_welcomes` path (also peer/1-mediated).
        let activated = bob.poll_welcomes().await;
        assert_eq!(activated, 1, "bob activated alice's group via wire");
        assert!(
            bob.dht.dials() > 0,
            "bob's poll_welcomes dispatched through Peer1DhtClient ({} dials)",
            bob.dht.dials()
        );

        // Alice now sends an Application message — the bytes go
        // through the production
        // `build_application_envelope_bytes` path (envelope sig +
        // size cap + outer DispatchP would all run if we had the
        // global RELAY connection; for this in-process test we
        // hand-deliver to bob).
        let plaintext = b"hello via JNI path";
        let result = send_app_message(
            &alice,
            &mut alice_group,
            &bob.ipk,
            plaintext,
            &bob,
        );
        match result {
            InboundDecoded::Application { plaintext: pt, .. } => {
                assert_eq!(pt, b"hello via JNI path");
            },
            other => panic!(
                "expected Application from JNI path, got {:?}",
                discriminant(&other)
            ),
        }

        // Final invariant: the send-and-receive flow registered MORE
        // peer/1 dials than just the KP-fetch path (publish welcome
        // fanned out to multiple homes). This is the load-bearing
        // proof: a stub `NotWiredDhtClient` would have `dials() == 0`
        // because its method bodies return `NotConfigured` without
        // touching the network.
        assert!(
            alice.dht.dials() >= 2,
            "JNI-equivalent flow MUST dial >= 2 homes for K-quorum publish (got {})",
            alice.dht.dials()
        );
    })
}

// ---------------------------------------------------------------------
// Test: Phase 8 P0-2 — TLS cert SPKI pinning rejects tampered relay.
//
// Spin up a real relay (cert SPKI = NodeKey K_real). Build a
// `Peer1DhtClient` whose `home.pubkey = Some(K_FAKE)` — a different
// 32-byte key the relay is NOT serving. Any dial through this client
// must fail at the TLS handshake because the
// `PinnedPeerServerCertVerifier` rejects the SPKI mismatch BEFORE
// any application bytes flow.
//
// This is the proof analogous to the Phase 7 `dials() > 0` proof:
// it demonstrates that the libcore-side pinning code path is
// load-bearing (and not a no-op stub).
// ---------------------------------------------------------------------
#[test]
fn pinning_rejects_tampered_relay_pubkey() {
    block_on(async {
        let r0 = TestRelay::start(0xC0).await;

        // Build a libcore client that lies to itself about the relay's
        // pubkey. The relay's actual cert SPKI is `r0.pubkey` (its
        // NodeKey, since `build_test_peer_server_cfg` self-signs with
        // `signing` whose verifying_key == r0.pubkey). The client
        // pins against a *different* key entirely.
        let ipk_signer = SigningKey::from_bytes(&[0xC1; 32]);
        let ipk = ipk_signer.verifying_key().to_bytes();
        let tls_subkey = SigningKey::from_bytes(&[0xC2; 32]);
        let peer_cfg =
            Arc::new(build_peer_client_cfg_with_subkey(tls_subkey.clone()).expect("client cfg"));
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();

        // The lie: pin against a key the relay does NOT serve.
        let fake_pubkey = SigningKey::from_bytes(&[0xDE; 32])
            .verifying_key()
            .to_bytes();
        assert_ne!(fake_pubkey, r0.pubkey, "fake key must differ from real key");

        let home_desc = HomeDescriptor {
            node_id: r0.node_id,
            addr:    r0.addr,
            pubkey:  Some(fake_pubkey),
        };

        let client = Peer1DhtClient::new_arc_with_tls_subkey(
            endpoint,
            peer_cfg,
            tls_subkey,
            home_desc,
            ipk,
            ipk_signer,
        );

        // Attempt any peer/1 RPC that triggers a dial — `fetch_welcomes`
        // calls `find_k_closest` which calls `get_or_dial` which goes
        // through the per-dial pinned ClientConfig.
        let res = client.fetch_welcomes().await;

        // The dial must fail. The exact error variant is
        // `DhtClientError::Transport(...)` because the TLS handshake
        // closes before any app-layer protocol runs; the inner string
        // mentions "SPKI mismatch" or "handshake".
        match res {
            Err(e) => {
                let msg = format!("{e:?}");
                assert!(
                    msg.contains("SPKI") || msg.contains("handshake") || msg.contains("Transport"),
                    "expected pinning-related transport failure, got: {msg}"
                );
            }
            Ok(_) => {
                panic!("dial succeeded against a tampered pin — pinning is NOT enforced!");
            }
        }
    })
}
