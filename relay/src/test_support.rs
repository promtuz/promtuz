//! Fixtures for the relay's tests: temp-dir stores, signed packets and production relays on
//! loopback.

use std::net::SocketAddr;
use std::sync::Arc;

use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SHandshakePacket;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::ServerHandshakeResultP;
use common::proto::client_rel::Wake;
use common::proto::client_rel::client_auth_message;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::dht_p2p::NodeDescriptor;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::MLS_ENVELOPE_VERSION;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WelcomeEnvelopeP;
use common::proto::mls_wire::kp_publish_records_digest;
use common::proto::mls_wire::kp_publish_signing_input;
use common::proto::mls_wire::kp_record_signing_input;
use common::proto::mls_wire::welcome_envelope_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::proto::pack::unpack_optional;
use common::quic::config::build_client_cfg;
use common::quic::id::NodeId;
use common::quic::protorole::ProtoRole;
use common::quic::tunnel;
use common::quic::tunnel::Channel;
use common::quic::tunnel_listener::NodeTunnel;
use common::quic::tunnel_listener::Running;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use quinn::Connection;
use quinn::Endpoint;
use quinn::EndpointConfig;
use quinn::TokioRuntime;
use rustls::RootCertStore;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::dht::Dht;
use crate::dht::DhtConfig;
use crate::quic::handler::Handler;
use crate::relay::Relay;
use crate::storage::MessageKey;
use crate::storage::db::Store;
use crate::util::config::AppConfig;

pub(crate) fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(crate) fn ipk(seed: u8) -> [u8; 32] {
    key(seed).verifying_key().to_bytes()
}

/// Bind it as `let (_dir, dht) = ...`, so the store closes before its directory goes.
pub(crate) fn dht(id: NodeId) -> (TempDir, Arc<Dht>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open_empty(dir.path()));
    (dir, Arc::new(Dht::new(id, key(0xD0), DhtConfig::default(), store)))
}

/// Key-bound, at an address this relay refuses to dial, so every RPC to it fails at once.
pub(crate) fn unreachable_peer(seed: u8) -> NodeDescriptor {
    NodeDescriptor {
        id:     NodeId::new(ipk(seed)),
        addr:   "127.0.0.1:9".parse().unwrap(),
        pubkey: ipk(seed).into(),
    }
}

pub(crate) fn dispatch(from: &SigningKey, to: [u8; 32], id: [u8; 16], payload: &[u8]) -> DispatchP {
    let from_ipk = from.verifying_key().to_bytes();
    let sig = from.sign(&dispatch_sig_message(&to, &from_ipk, &id, payload)).to_bytes();
    DispatchP {
        to:             to.into(),
        from:           from_ipk.into(),
        id:             id.into(),
        payload:        payload.to_vec().into(),
        sig:            sig.into(),
        accepted_at_ms: 1,
        wake:           Wake::No,
        ttl_ms:         0,
    }
}

/// A home-queue row written directly: no admission scan and no commit.
pub(crate) fn put_queued(dht: &Dht, user: &[u8; 32], at: u64, row: &DispatchP) {
    let key = MessageKey::new(user, at, &row.id.0);
    dht.store.queue.insert(key.as_bytes(), row.ser().unwrap()).unwrap();
}

/// Ids in `user`'s home queue, oldest first.
pub(crate) fn queued(dht: &Dht, user: &[u8; 32]) -> Vec<[u8; 16]> {
    dht.store
        .queue
        .prefix(user)
        .map(|row| MessageKey::parse(&row.key().unwrap()).unwrap().id)
        .collect()
}

pub(crate) fn kp_record(
    owner: &SigningKey, kp_ref: [u8; 32], expires_at_ms: u64,
) -> KeyPackageRecord {
    let ipk = owner.verifying_key().to_bytes();
    let kp_bytes = kp_ref.to_vec();
    let msg = kp_record_signing_input(MLS_WIRE_VERSION, &ipk, &kp_ref, &kp_bytes, expires_at_ms);
    KeyPackageRecord {
        ipk: ipk.into(),
        kp_ref: kp_ref.to_vec().into(),
        kp_bytes: kp_bytes.into(),
        expires_at_ms,
        owner_sig: owner.sign(&msg).to_bytes().into(),
    }
}

pub(crate) fn kp_sig(owner: &SigningKey, records: &[KeyPackageRecord], timestamp: u64) -> [u8; 64] {
    let ipk = owner.verifying_key().to_bytes();
    let digest = kp_publish_records_digest(MLS_WIRE_VERSION, records);
    let count = records.len() as u32;
    let msg = kp_publish_signing_input(MLS_WIRE_VERSION, &ipk, &digest, count, timestamp);
    owner.sign(&msg).to_bytes()
}

/// `tag` makes the group and KeyPackage distinct, so two envelopes are two Welcomes.
pub(crate) fn welcome(sender: &SigningKey, recipient: [u8; 32], tag: u8) -> WelcomeEnvelopeP {
    let sender_ipk = sender.verifying_key().to_bytes();
    let (group_id, kp_ref_used, blob) = ([tag; 32], [!tag; 32], vec![tag; 8]);
    let msg = welcome_envelope_signing_input(
        MLS_WIRE_VERSION,
        &group_id,
        &sender_ipk,
        &recipient,
        &kp_ref_used,
        &blob,
    );
    WelcomeEnvelopeP {
        version:       MLS_ENVELOPE_VERSION,
        group_id:      group_id.into(),
        sender_ipk:    sender_ipk.into(),
        recipient_ipk: recipient.into(),
        welcome_blob:  blob.into(),
        kp_ref_used:   kp_ref_used.into(),
        sender_sig:    sender.sign(&msg).to_bytes().into(),
        pairing:       None,
    }
}

/// A production relay on loopback: QUIC on one port, the phone TLS carrier on another, and a
/// self-signed `localhost` certificate as its CA.
pub(crate) struct Node {
    pub relay:  Arc<Relay>,
    pub tunnel: SocketAddr,
    pub roots:  RootCertStore,
    cancel:     CancellationToken,
    _tunnel:    Running,
    _dir:       TempDir,
}

impl Node {
    pub(crate) async fn start(seed: u8, dht: bool) -> Self {
        // `PZ_LOG` still overrides this for a run that needs the relay's logs.
        common::server::log::init(Some("error"));
        let _ = common::quic::config::setup_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let signing = key(seed);
        let pkcs8 = signing.to_pkcs8_der().unwrap();
        let key_pair = rcgen::KeyPair::try_from(pkcs8.as_bytes()).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        let cert_path = dir.path().join("relay.pem");
        let key_path = dir.path().join("relay.key");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();

        let cfg = AppConfig {
            network:        NetworkConfig {
                address: "127.0.0.1:0".parse().unwrap(),
                cert_path: cert_path.clone(),
                key_path,
                root_ca_path: cert_path,
                watch_reload: false,
                tcp_fallback: true,
            },
            resolver:       NodeConfig { seed: vec![] },
            control_socket: dir.path().join("control.sock"),
            dht:            DhtConfig { enabled: dht, allow_local_peer_addrs: true },
            assist:         Default::default(),
            turn:           Default::default(),
            log:            Default::default(),
        };
        let listener =
            NodeTunnel::bind(&cfg.network, tunnel::FEATURE_CONTROL).await.unwrap().unwrap();
        let store = Arc::new(Store::open_empty(&dir.path().join("store")));
        let bound = Relay::bind(&cfg, &signing);
        let relay = Arc::new(Relay::new(cfg, &signing, bound, store));

        let cancel = CancellationToken::new();
        let handle = {
            let (relay, cancel) = (relay.clone(), cancel.clone());
            move |connection| Handler::handle(connection, relay.clone(), cancel.clone())
        };
        tokio::spawn(common::server::accept::serve(
            relay.endpoint.clone(),
            crate::ACCEPT,
            handle.clone(),
        ));
        let tunnel = listener.local_addr().unwrap();
        let running = listener.spawn(handle, |channel, _| async move { channel.close() });
        Self { relay, tunnel, roots, cancel, _tunnel: running, _dir: dir }
    }

    pub(crate) fn dht(&self) -> &Arc<Dht> {
        self.relay.dht.as_ref().expect("started with a DHT")
    }

    /// What a resolver bootstrap would have told this relay about `other`.
    pub(crate) fn learn(&self, other: &Node) {
        let pubkey = other.dht().signing_key.verifying_key().to_bytes();
        self.dht().routing.write().insert(NodeDescriptor {
            id:     other.dht().node_id,
            addr:   other.relay.endpoint.local_addr().unwrap(),
            pubkey: pubkey.into(),
        });
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.relay.endpoint.close(0u32.into(), b"test finished");
    }
}

/// A phone on the TLS carrier, so it cannot send the relay a single UDP datagram.
pub(crate) struct Client {
    pub connection: Connection,
    channel:        Arc<Channel>,
    endpoint:       Endpoint,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.channel.close();
        self.endpoint.close(0u32.into(), b"test client closed");
    }
}

impl Client {
    pub(crate) async fn connect(node: &Node) -> Self {
        let channel =
            tunnel::connect(node.tunnel, "localhost", &node.roots, tunnel::Request::Control)
                .await
                .unwrap();
        let socket = channel.clone().socket(channel.local_addr(), channel.peer_addr());
        let mut endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            socket,
            Arc::new(TokioRuntime),
        )
        .unwrap();
        endpoint
            .set_default_client_config(build_client_cfg(ProtoRole::Client, &node.roots).unwrap());
        let connection = endpoint.connect(node.tunnel, "localhost").unwrap().await.unwrap();
        Self { connection, channel, endpoint }
    }

    /// Answers the challenge with `proof` over `session`'s exporter; `true` if the relay accepts.
    pub(crate) async fn authenticate(
        &self, identity: &SigningKey, proof: &SigningKey, session: &Connection,
    ) -> bool {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        let hello = CHandshakePacket::Hello { ipk: identity.verifying_key().to_bytes().into() };
        send.write_all(&hello.pack().unwrap()).await.unwrap();
        let Ok(SHandshakePacket::Challenge { nonce }) =
            SHandshakePacket::unpack(&mut receive).await
        else {
            return false;
        };
        let binding = common::quic::client_auth_binding(session).unwrap();
        let sig = proof.sign(&client_auth_message(&nonce, &binding)).to_bytes();
        send.write_all(&CHandshakePacket::Proof { sig: sig.into() }.pack().unwrap()).await.unwrap();
        let _ = send.finish();
        matches!(
            SHandshakePacket::unpack(&mut receive).await,
            Ok(SHandshakePacket::HandshakeResult(ServerHandshakeResultP::Accept { .. }))
        )
    }

    /// Sends `packets` on one stream and returns every reply until the relay ends it.
    pub(crate) async fn request(&self, packets: Vec<CRelayPacket>) -> Vec<SRelayPacket> {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        for packet in packets {
            send.write_all(&packet.pack().unwrap()).await.unwrap();
        }
        send.finish().unwrap();
        let mut replies = Vec::new();
        while let Some(reply) = unpack_optional(&mut receive).await.unwrap() {
            replies.push(reply);
        }
        replies
    }
}
