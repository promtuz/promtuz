//! Production relay handlers over real TLS control pipes. Payloads are opaque
//! signed test bytes, not an MLS/app-account fixture; no external nodes run.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::node::config::{NetworkConfig, NodeConfig};
use common::proto::client_rel::{
    CHandshakePacket, CRelayPacket, DeliverP, DispatchAckP, DispatchP, QueryP, SHandshakePacket,
    SRelayPacket, ServerHandshakeResultP, Wake, client_auth_message, dispatch_sig_message,
};
use common::proto::pack::{Packer, Unpacker};
use common::quic::config::build_client_cfg;
use common::quic::id::NodeKey;
use common::quic::protorole::ProtoRole;
use common::quic::tunnel::{self, Channel};
use common::quic::tunnel_listener::{NodeTunnel, Running};
use ed25519_dalek::{Signer, SigningKey, pkcs8::EncodePrivateKey};
use parking_lot::{Mutex, RwLock};
use quinn::{Connection, Endpoint, EndpointConfig, TokioRuntime};
use tokio_util::sync::CancellationToken;

use crate::relay::{Relay, RelayKeys};
use crate::storage::db::Store;
use crate::util::config::{AppConfig, AssistConfig, LogConfig, TurnConfig};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "promtuz-tcp-control-{}-{:016x}",
            std::process::id(),
            rand::random::<u64>(),
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    relay: Arc<Relay>,
    address: std::net::SocketAddr,
    roots: rustls::RootCertStore,
    listener: Option<Running>,
    cancel: CancellationToken,
    // Declared last so owned store/connection handles are dropped first.
    _scratch: Scratch,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.relay.endpoint.close(0u32.into(), b"test finished");
    }
}

impl Server {
    async fn new() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let scratch = Scratch::new();
        let signing = SigningKey::from_bytes(&[31; 32]);
        let public = signing.verifying_key();
        let private = signing.to_pkcs8_der().unwrap();
        let key_pair = rcgen::KeyPair::try_from(private.as_bytes()).unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        let cert_path = scratch.0.join("relay.pem");
        let key_path = scratch.0.join("relay.key");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let cfg = AppConfig {
            network: NetworkConfig {
                address: "127.0.0.1:0".parse().unwrap(),
                cert_path,
                key_path,
                root_ca_path: PathBuf::new(),
                watch_reload: false,
                tcp_fallback: true,
            },
            resolver: NodeConfig { seed: vec![] },
            control_socket: scratch.0.join("unused.sock"),
            dht: Default::default(),
            assist: AssistConfig::default(),
            turn: TurnConfig::default(),
            log: LogConfig::default(),
        };
        let listener =
            NodeTunnel::bind(&cfg.network, tunnel::FEATURE_CONTROL).await.unwrap().unwrap();
        let address = listener.local_addr().unwrap();
        let client_cfg = Arc::new(build_client_cfg(ProtoRole::Client, &roots).unwrap());
        // Required by Relay's ordinary DHT machinery, disabled in this fixture.
        // It is never used for these clients; every client below has a TLS
        // abstract socket and cannot emit UDP at all.
        let endpoint = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let relay = Arc::new(Relay {
            key: NodeKey::new(public).unwrap(),
            keys: RelayKeys { signing, public },
            endpoint,
            assist: Mutex::new(None),
            assist_enabled: false,
            turn: None,
            cfg,
            client_cfg,
            store: Arc::new(Store::open(scratch.0.join("store")).unwrap()),
            dht: None,
            clients: Arc::new(RwLock::new(HashMap::new())),
            presence_subs: RwLock::new(HashMap::<_, HashSet<_>>::new()),
            presence_leases: Arc::new(RwLock::new(HashMap::new())),
            presence_versions: RwLock::new(HashMap::new()),
            active_clients: RwLock::new(HashMap::new()),
            push_pseudonyms: Arc::new(RwLock::new(HashMap::new())),
        });
        let cancel = CancellationToken::new();
        let listener =
            Some(listener.spawn(
                {
                    let relay = relay.clone();
                    let cancel = cancel.clone();
                    move |connection| {
                        let relay = relay.clone();
                        let cancel = cancel.clone();
                        async move {
                            crate::quic::handler::Handler::handle(connection, relay, cancel).await
                        }
                    }
                },
                |channel, _| async move {
                    channel.close();
                },
            ));
        Self { relay, address, roots, listener, cancel, _scratch: scratch }
    }

    async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(listener) = self.listener.take() {
            listener.shutdown().await;
        }
    }
}

struct Client {
    channel: Arc<Channel>,
    connection: Connection,
    endpoint: Endpoint,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.channel.close();
        self.endpoint.close(0u32.into(), b"test client closed");
    }
}

impl Client {
    async fn connect(server: &Server) -> Self {
        let channel =
            tunnel::connect(server.address, "localhost", &server.roots, tunnel::Request::Control)
                .await
                .unwrap();
        let mut endpoint = Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            channel.clone().socket(channel.local_addr(), channel.peer_addr()),
            Arc::new(TokioRuntime),
        )
        .unwrap();
        endpoint
            .set_default_client_config(build_client_cfg(ProtoRole::Client, &server.roots).unwrap());
        let connection = endpoint.connect(server.address, "localhost").unwrap().await.unwrap();
        Self { channel, connection, endpoint }
    }

    async fn authenticate(&self, identity: &SigningKey, proof_key: &SigningKey) -> bool {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        packet(
            &mut send,
            &CHandshakePacket::Hello { ipk: identity.verifying_key().to_bytes().into() },
        )
        .await;
        let SHandshakePacket::Challenge { nonce } =
            SHandshakePacket::unpack(&mut receive).await.unwrap()
        else {
            panic!("missing challenge")
        };
        let binding = common::quic::client_auth_binding(&self.connection).unwrap();
        let sig = proof_key.sign(&client_auth_message(&nonce, &binding)).to_bytes();
        packet(&mut send, &CHandshakePacket::Proof { sig: sig.into() }).await;
        send.finish().unwrap();
        matches!(
            SHandshakePacket::unpack(&mut receive).await,
            Ok(SHandshakePacket::HandshakeResult(ServerHandshakeResultP::Accept { .. }))
        )
    }

    async fn barrier(&self) {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        packet(&mut send, &CRelayPacket::Query(QueryP::PubAddress)).await;
        send.finish().unwrap();
        assert!(matches!(
            SRelayPacket::unpack(&mut receive).await.unwrap(),
            SRelayPacket::QueryResult(_)
        ));
    }

    async fn dispatch(&self, dispatch: DispatchP) -> DispatchAckP {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        packet(&mut send, &CRelayPacket::Dispatch(dispatch)).await;
        send.finish().unwrap();
        let SRelayPacket::DispatchAck(ack) = SRelayPacket::unpack(&mut receive).await.unwrap()
        else {
            panic!("missing dispatch ack")
        };
        ack
    }

    async fn drain(&self) -> DeliverP {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        packet(&mut send, &CRelayPacket::DrainQueue).await;
        send.finish().unwrap();
        let SRelayPacket::Deliver(delivery) = SRelayPacket::unpack(&mut receive).await.unwrap()
        else {
            panic!("missing queued delivery")
        };
        assert!(receive.read_to_end(1024).await.unwrap().is_empty());
        delivery
    }

    async fn ack_drain(&self) {
        let (mut send, mut receive) = self.connection.open_bi().await.unwrap();
        packet(&mut send, &CRelayPacket::AckDrain).await;
        // Same stream processes this query after the preceding AckDrain.
        packet(&mut send, &CRelayPacket::Query(QueryP::PubAddress)).await;
        send.finish().unwrap();
        assert!(matches!(
            SRelayPacket::unpack(&mut receive).await.unwrap(),
            SRelayPacket::QueryResult(_)
        ));
    }
}

async fn packet<T: serde::Serialize>(stream: &mut quinn::SendStream, packet: &T) {
    stream.write_all(&packet.pack().unwrap()).await.unwrap();
}

fn dispatch(from: &SigningKey, to: &SigningKey, sequence: u8, payload: &[u8]) -> DispatchP {
    let from_ipk = from.verifying_key().to_bytes();
    let to_ipk = to.verifying_key().to_bytes();
    let id = [sequence; 16];
    let sig = from.sign(&dispatch_sig_message(&to_ipk, &from_ipk, &id, payload)).to_bytes();
    DispatchP {
        to: to_ipk.into(),
        from: from_ipk.into(),
        id: id.into(),
        payload: payload.to_vec().into(),
        sig: sig.into(),
        accepted_at_ms: 0,
        wake: Wake::No,
        ttl_ms: 0,
    }
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn one_relay_authenticates_delivers_and_drains_over_tls_without_client_udp() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let server = Server::new().await;
        let a_key = SigningKey::from_bytes(&[51; 32]);
        let b_key = SigningKey::from_bytes(&[52; 32]);
        let attacker = Client::connect(&server).await;
        assert!(!attacker.authenticate(&a_key, &b_key).await);
        assert!(server.relay.clients.read().is_empty(), "bad proof never registers an IPK");
        drop(attacker);

        let a = Client::connect(&server).await;
        let b = Client::connect(&server).await;
        assert!(a.authenticate(&a_key, &a_key).await);
        assert!(b.authenticate(&b_key, &b_key).await);
        a.barrier().await;
        b.barrier().await;
        assert_eq!(server.relay.clients.read().len(), 2);

        // Even a valid signature from B cannot be submitted under A's session.
        let impersonated = dispatch(&b_key, &a_key, 1, b"not my authenticated identity");
        assert_eq!(a.dispatch(impersonated).await, DispatchAckP::InvalidSig);
        assert_eq!(server.relay.store.messages.len().unwrap(), 0);

        let live = dispatch(&a_key, &b_key, 2, b"opaque encrypted offer stand-in");
        let expected = live.clone();
        assert!(matches!(a.dispatch(live).await, DispatchAckP::Queued { .. }));
        let (mut ack, mut incoming) = b.connection.accept_bi().await.unwrap();
        let SRelayPacket::Deliver(delivery) = SRelayPacket::unpack(&mut incoming).await.unwrap()
        else {
            panic!("missing live delivery")
        };
        assert_eq!(delivery.id, expected.id);
        assert_eq!(delivery.from, expected.from);
        assert_eq!(delivery.payload, expected.payload);
        assert_eq!(delivery.sig, expected.sig);
        assert!(delivery.accepted_at_ms > 0);
        packet(&mut ack, &CRelayPacket::DeliverAck).await;
        ack.finish().unwrap();
        eventually(|| server.relay.store.messages.len().unwrap() == 0).await;

        // TLS loss must deregister B promptly and push its next dispatch into
        // the durable local queue on this sole relay.
        drop(b);
        eventually(|| !server.relay.clients.read().contains_key(&b_key.verifying_key().to_bytes()))
            .await;
        let offline = dispatch(&a_key, &b_key, 3, b"opaque reverse wake stand-in");
        assert!(matches!(a.dispatch(offline.clone()).await, DispatchAckP::Queued { .. }));
        assert_eq!(server.relay.store.messages.len().unwrap(), 1);

        let b = Client::connect(&server).await;
        assert!(b.authenticate(&b_key, &b_key).await);
        b.barrier().await;
        let drained = b.drain().await;
        assert_eq!(drained.id, offline.id);
        assert_eq!(drained.payload, offline.payload);
        assert_eq!(drained.sig, offline.sig);
        assert_eq!(server.relay.store.messages.len().unwrap(), 1, "drain alone retains custody");
        assert_eq!(b.drain().await, drained, "unacknowledged drain remains recoverable");
        b.ack_drain().await;
        assert_eq!(server.relay.store.messages.len().unwrap(), 0);
        drop(a);
        drop(b);
        server.shutdown().await;
    })
    .await
    .expect("one-relay TCP control integration timed out");
}
