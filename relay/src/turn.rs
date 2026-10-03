//! A standard TURN server (RFC 5766) for calls on its own UDP port. Credentials are coturn-style:
//! the username holds expiry and client, the password is its HMAC under a relay-key-derived secret.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use common::info;
use common::proto::client_rel::TurnCredentialsP;
use common::warn;
use ed25519_dalek::SigningKey;
use parking_lot::Mutex;
use ring::hmac;
use tokio_util::sync::CancellationToken;
use turn::auth::AuthHandler;
use turn::auth::generate_auth_key;
use turn::relay::RelayAddressGenerator;
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::ConnConfig;
use turn::server::config::ServerConfig;
use webrtc_util::Conn;
use webrtc_util::vnet::net::Net;

use crate::util::config::TurnConfig;

const REALM: &str = "promtuz";
/// Long enough to set up any call. Refreshes are authenticated too, so an allocation cannot be
/// renewed past it.
const CREDENTIAL_LIFETIME: Duration = Duration::from_secs(60 * 60);

pub struct Turn {
    pub port: u16,
    public_ip: IpAddr,
    allow_local_peers: bool,
    secret: hmac::Key,
    socket: Mutex<Option<std::net::UdpSocket>>,
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn").field("port", &self.port).field("public_ip", &self.public_ip).finish()
    }
}

impl Turn {
    /// Binds now, so a taken port fails at startup.
    pub fn bind(cfg: &TurnConfig, signing: &SigningKey) -> Result<Self> {
        let public_ip = cfg.public_ip.context("[turn] public_ip is required when enabled")?;
        let socket = std::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], cfg.port)))
            .with_context(|| format!("binding the TURN socket on port {}", cfg.port))?;
        socket.set_nonblocking(true)?;
        let root = hmac::Key::new(hmac::HMAC_SHA256, &signing.to_bytes());
        let secret =
            hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&root, b"promtuz turn secret v1").as_ref());
        Ok(Self {
            port: cfg.port,
            public_ip,
            allow_local_peers: cfg.allow_local_peers,
            secret,
            socket: Mutex::new(Some(socket)),
        })
    }

    pub fn credentials(&self, client: &[u8; 32], now_ms: u64) -> TurnCredentialsP {
        let expires_at_ms = now_ms + CREDENTIAL_LIFETIME.as_millis() as u64;
        let username = format!("{}:{}", expires_at_ms / 1_000, hex::encode(&client[..4]));
        let password = self.password_for(&username);
        TurnCredentialsP { username, password, expires_at_ms }
    }

    fn password_for(&self, username: &str) -> String {
        base64::engine::general_purpose::STANDARD
            .encode(hmac::sign(&self.secret, username.as_bytes()).as_ref())
    }

    pub async fn serve(self: Arc<Self>, cancel: CancellationToken) {
        let Some(std_sock) = self.socket.lock().take() else { return };
        let sock = match tokio::net::UdpSocket::from_std(std_sock) {
            Ok(s) => s,
            Err(e) => return warn!("TURN socket unusable: {e}"),
        };
        let config = ServerConfig {
            conn_configs: vec![ConnConfig {
                conn: Arc::new(sock),
                relay_addr_generator: Box::new(PublicPeers {
                    inner: RelayAddressGeneratorStatic {
                        relay_address: self.public_ip,
                        address: "0.0.0.0".into(),
                        net: Arc::new(Net::new(None)),
                    },
                    allow_local: self.allow_local_peers,
                }),
            }],
            realm: REALM.into(),
            auth_handler: self.clone(),
            channel_bind_timeout: Duration::from_secs(0),
            alloc_close_notify: None,
        };
        let server = match Server::new(config).await {
            Ok(s) => s,
            Err(e) => return warn!("TURN server failed to start: {e}"),
        };
        info!("TURN listening at UDP(0.0.0.0:{}) as {}", self.port, self.public_ip);
        cancel.cancelled().await;
        let _ = server.close().await;
    }
}

impl AuthHandler for Turn {
    /// Only expiry is checked here: a forged username or wrong password fails the TURN server's
    /// message-integrity check against the key returned.
    fn auth_handle(&self, username: &str, realm: &str, _src: SocketAddr) -> Result<Vec<u8>, turn::Error> {
        let expiry: u64 = username
            .split(':')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| turn::Error::Other("malformed TURN username".into()))?;
        if expiry < common::utils::now_secs() {
            return Err(turn::Error::Other("expired TURN credentials".into()));
        }
        Ok(generate_auth_key(username, realm, &self.password_for(username)))
    }
}

/// Relayed sockets that drop packets for loopback, private and link-local
/// peers, so an allocation cannot reach hosts behind this relay.
struct PublicPeers {
    inner: RelayAddressGeneratorStatic,
    allow_local: bool,
}

#[async_trait]
impl RelayAddressGenerator for PublicPeers {
    fn validate(&self) -> Result<(), turn::Error> {
        self.inner.validate()
    }

    async fn allocate_conn(
        &self, use_ipv4: bool, requested_port: u16,
    ) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        let (conn, addr) = self.inner.allocate_conn(use_ipv4, requested_port).await?;
        Ok((Arc::new(PublicOnly { conn, allow_local: self.allow_local }), addr))
    }
}

struct PublicOnly {
    conn: Arc<dyn Conn + Send + Sync>,
    allow_local: bool,
}

#[async_trait]
impl Conn for PublicOnly {
    async fn connect(&self, addr: SocketAddr) -> Result<(), webrtc_util::Error> {
        self.conn.connect(addr).await
    }

    async fn recv(&self, buf: &mut [u8]) -> Result<usize, webrtc_util::Error> {
        self.conn.recv(buf).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), webrtc_util::Error> {
        self.conn.recv_from(buf).await
    }

    async fn send(&self, buf: &[u8]) -> Result<usize, webrtc_util::Error> {
        self.conn.send(buf).await
    }

    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize, webrtc_util::Error> {
        if !crate::dht::lookup::is_dialable_peer_addr(&target, self.allow_local) {
            return Ok(buf.len());
        }
        self.conn.send_to(buf, target).await
    }

    fn local_addr(&self) -> Result<SocketAddr, webrtc_util::Error> {
        self.conn.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.conn.remote_addr()
    }

    async fn close(&self) -> Result<(), webrtc_util::Error> {
        self.conn.close().await
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}
