use std::sync::Arc;

use common::debug;
use common::error;
use common::info;
use common::proto::RelayId;
use common::proto::pack::Unpacker;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::ResolverPacket;
use common::quic::CloseReason;
use common::utils::now_ms;
use common::warn;
use parking_lot::Mutex;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

pub(super) trait HandleRelay {
    async fn handle_relay(self, resolver: ResolverRef);
}

impl HandleRelay for Handler {
    async fn handle_relay(self, resolver: ResolverRef) {
        let conn = self.conn.clone();
        tokio::join!(
            lifecycle_loop(conn.clone(), resolver.clone()),
            super::client::serve_rpc_streams(conn, resolver),
        );
    }
}

async fn lifecycle_loop(conn: Arc<Connection>, resolver: ResolverRef) {
    let addr = conn.remote_address();
    let session = Arc::new(Session::default());

    loop {
        let mut recv = match conn.accept_uni().await {
            Ok(recv) => recv,
            Err(err) => {
                debug!("relay({addr}) stream accept ended: {err}");
                break;
            },
        };

        let conn = conn.clone();
        let resolver = resolver.clone();
        let session = session.clone();

        tokio::spawn(async move {
            while let Ok(ResolverPacket::Lifetime(packet)) = ResolverPacket::unpack(&mut recv).await
            {
                if let Err(close) = handle_lifetime(&conn, &resolver, &session, packet).await {
                    close.close(&conn);
                    return;
                }
            }
        });
    }
}

/// The one identity a connection may register: it bounds a session to a single directory slot
/// and the close watcher to one spawn per connection.
#[derive(Default)]
struct Session(Mutex<Option<RelayId>>);

enum Claim {
    First,
    Repeat,
    Conflict,
}

impl Session {
    fn claim(&self, id: RelayId) -> Claim {
        let mut held = self.0.lock();
        match *held {
            None => {
                *held = Some(id);
                Claim::First
            },
            Some(existing) if existing == id => Claim::Repeat,
            Some(_) => Claim::Conflict,
        }
    }
}

/// An `Err` is the reason to close the connection with.
async fn handle_lifetime(
    conn: &Arc<Connection>, resolver: &ResolverRef, session: &Session, packet: LifetimeP,
) -> Result<(), CloseReason> {
    let addr = conn.remote_address();

    use LifetimeP::*;
    match packet {
        RelayHello { relay_id: id, .. } | GatewayHello { gateway_id: id, .. } => {
            match session.claim(id) {
                Claim::First => {},
                // Re-admitting would re-verify and rebuild the cached `GetRelays` response.
                Claim::Repeat => return Ok(()),
                Claim::Conflict => return Err(CloseReason::AlreadyConnected),
            }

            let dir = resolver.register(conn, &packet)?;
            dir.watch(id, conn.clone());

            let ack = async {
                let mut send = conn.open_uni().await?;
                ResolverPacket::Lifetime(HelloAck { resolver_time: now_ms().into() })
                    .send(&mut send)
                    .await?;
                anyhow::Ok(send.finish()?)
            };
            match ack.await {
                Ok(()) => info!("{}({addr}) connected with ID({id})", dir.kind),
                Err(e) => error!("{}({addr}) hello ack failed: {e}", dir.kind),
            }
            Ok(())
        },
        RelayHeartbeat { .. } => resolver.verify_heartbeat(conn, &packet),
        _ => {
            warn!("unexpected lifetime packet from relay({addr})");
            Ok(())
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use common::crypto::SigningKey;
    use common::node::config::NetworkConfig;
    use common::proto::relay_res::NODE_HELLO_EXPORTER_LABEL;
    use common::proto::relay_res::relay_heartbeat_signing_input;
    use common::proto::relay_res::relay_hello_signing_input;
    use common::quic::config::Ed25519CertVerifier;
    use common::quic::config::build_self_signed_ed25519_cert;
    use common::quic::config::setup_crypto_provider;
    use common::quic::id::NodeId;
    use common::quic::protorole::ProtoRole;
    use common::quic::session_binding;
    use common::server::log::LogConfig;
    use common::sysutils::SystemLoad;
    use common::types::bytes::Bytes;
    use ed25519_dalek::Signer as _;
    use quinn::ConnectionError;
    use quinn::Endpoint;
    use quinn::crypto::rustls::QuicClientConfig;
    use quinn::crypto::rustls::QuicServerConfig;
    use quinn::rustls;
    use serde::Deserialize as _;
    use serde::de::value::SeqDeserializer;

    use super::*;
    use crate::resolver::Resolver;
    use crate::resolver::relays::Directory;
    use crate::util::config::AppConfig;

    struct Net {
        server: Endpoint,
        client: Endpoint,
    }

    impl Net {
        fn new() -> Self {
            let _ = setup_crypto_provider();
            let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
            let alpn = vec![ProtoRole::Relay.alpn().into_bytes()];
            let cert = build_self_signed_ed25519_cert(SigningKey::from_bytes(&[1; 32]));
            let mut tls = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(cert)));
            tls.alpn_protocols = alpn.clone();
            let crypto = Arc::new(QuicServerConfig::try_from(tls).unwrap());
            let server =
                Endpoint::server(quinn::ServerConfig::with_crypto(crypto), loopback).unwrap();
            let mut tls = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(Ed25519CertVerifier))
                .with_no_client_auth();
            tls.alpn_protocols = alpn;
            let crypto = Arc::new(QuicClientConfig::try_from(tls).unwrap());
            let mut client = Endpoint::client(loopback).unwrap();
            client.set_default_client_config(quinn::ClientConfig::new(crypto));
            Self { server, client }
        }

        async fn connect(&self) -> (Connection, Arc<Connection>) {
            let to = self.server.local_addr().unwrap();
            let (client, server) = tokio::join!(
                async { self.client.connect(to, "resolver").unwrap().await.unwrap() },
                async { self.server.accept().await.unwrap().await.unwrap() },
            );
            (client, Arc::new(server))
        }

        fn resolver(&self) -> ResolverRef {
            let unused = PathBuf::new();
            let network = NetworkConfig {
                address: self.server.local_addr().unwrap(),
                cert_path: unused.clone(),
                key_path: unused.clone(),
                root_ca_path: unused,
                watch_reload: false,
                tcp_fallback: false,
            };
            let cfg = AppConfig { network, log: LogConfig::default(), store: Vec::new() };
            Arc::new(Resolver::new(cfg, self.server.clone()))
        }
    }

    fn hello(key: &SigningKey, session: &Connection) -> LifetimeP {
        let pubkey = key.verifying_key().to_bytes();
        let relay_id = NodeId::new(pubkey);
        let timestamp = now_ms().into();
        let binding = session_binding(session, NODE_HELLO_EXPORTER_LABEL).unwrap();
        let sig = key.sign(&relay_hello_signing_input(&relay_id, &pubkey, timestamp, &binding));
        LifetimeP::RelayHello {
            relay_id,
            pubkey: Bytes(pubkey),
            timestamp,
            sig: Bytes(sig.to_bytes()),
        }
    }

    fn heartbeat(key: &SigningKey) -> LifetimeP {
        let pubkey = key.verifying_key().to_bytes();
        let relay_id = NodeId::new(pubkey);
        let timestamp = now_ms().into();
        let sig = key.sign(&relay_heartbeat_signing_input(&relay_id, &pubkey, timestamp));
        // `SystemLoad` has no public constructor.
        let fields = SeqDeserializer::<_, serde::de::value::Error>::new([0u8; 2].into_iter());
        let load = SystemLoad::deserialize(fields).unwrap();
        LifetimeP::RelayHeartbeat {
            relay_id,
            pubkey: Bytes(pubkey),
            timestamp,
            sig: Bytes(sig.to_bytes()),
            load,
            uptime_seconds: 0,
        }
    }

    #[tokio::test]
    async fn the_latest_session_owns_a_relay_id() {
        let net = Net::new();
        let resolver = net.resolver();
        let key = SigningKey::from_bytes(&[7; 32]);
        let id = NodeId::new(key.verifying_key().to_bytes());
        let entry =
            || resolver.relays.snapshot().into_iter().find(|e| e.id == id).expect("dropped");
        let generation = || resolver.relays.generation.load(Ordering::Acquire);
        let fresh = Session::default;

        let (a, a_conn) = net.connect().await;
        let a_session = fresh();
        handle_lifetime(&a_conn, &resolver, &a_session, hello(&key, &a)).await.unwrap();
        let ack = ResolverPacket::unpack(&mut a.accept_uni().await.unwrap()).await.unwrap();
        assert!(matches!(ack, ResolverPacket::Lifetime(LifetimeP::HelloAck { .. })));
        handle_lifetime(&a_conn, &resolver, &a_session, heartbeat(&key)).await.unwrap();
        let registered = generation();
        handle_lifetime(&a_conn, &resolver, &a_session, hello(&key, &a)).await.unwrap();
        assert_eq!(generation(), registered, "a repeated hello re-registers");
        assert!(entry().slot().established, "a repeated hello resets liveness");
        let other = hello(&SigningKey::from_bytes(&[8; 32]), &a);
        let refused = handle_lifetime(&a_conn, &resolver, &a_session, other).await;
        assert!(matches!(refused, Err(CloseReason::AlreadyConnected)));

        let (b, b_conn) = net.connect().await;
        handle_lifetime(&b_conn, &resolver, &fresh(), hello(&key, &b)).await.unwrap();
        assert!(a_conn.close_reason().is_some(), "a new session did not supersede the old one");
        let reconnecting = CloseReason::Reconnecting.code();
        assert!(matches!(
            a.closed().await,
            ConnectionError::ApplicationClosed(close) if close.error_code == reconnecting
        ));
        resolver.relays.watch(id, a_conn.clone()).await.unwrap();
        assert!(Arc::ptr_eq(&entry().conn, &b_conn), "the old session's close removed the new one");

        let (_c, c_conn) = net.connect().await;
        let replayed = handle_lifetime(&c_conn, &resolver, &fresh(), heartbeat(&key)).await;
        assert!(matches!(replayed, Err(CloseReason::PacketMismatch)));
        let replayed = handle_lifetime(&c_conn, &resolver, &fresh(), hello(&key, &b)).await;
        assert!(matches!(replayed, Err(CloseReason::BadSignature)));
        assert!(Arc::ptr_eq(&entry().conn, &b_conn));
        assert!(!entry().slot().established);
        handle_lifetime(&b_conn, &resolver, &fresh(), heartbeat(&key)).await.unwrap();
        assert!(entry().slot().established);
    }

    #[tokio::test]
    async fn a_full_gateway_directory_keeps_its_gateways() {
        let net = Net::new();
        let gateways = Directory::new("gateway", 1, false);
        let ((_x, first), (_y, second)) = (net.connect().await, net.connect().await);
        gateways.admit(NodeId::new([1; 32]), Bytes([1; 32]), first.clone()).unwrap();
        let refused = gateways.admit(NodeId::new([2; 32]), Bytes([2; 32]), second);
        assert!(matches!(refused, Err(CloseReason::RegistryFull)));
        assert!(first.close_reason().is_none());
        let ids: Vec<RelayId> = gateways.snapshot().iter().map(|e| e.id).collect();
        assert_eq!(ids, [NodeId::new([1; 32])]);
    }
}
