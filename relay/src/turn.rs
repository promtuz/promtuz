//! TURN for calls: a standard TURN server (RFC 5766) on its own UDP port, so
//! a phone's ICE agent can allocate a relayed candidate here the same way it
//! would at any TURN server. Unlike the blind bridge in [`crate::stunturn`],
//! nothing is forwarded without credentials.
//!
//! Credentials are the coturn time-limited kind: the username carries an
//! expiry and the asking client, the password is an HMAC over it under a
//! secret derived from the relay's key. Nothing is stored per client and a
//! restart keeps every outstanding credential valid.

use std::net::IpAddr;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
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
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::ConnConfig;
use turn::server::config::ServerConfig;
use webrtc_util::vnet::net::Net;

use crate::util::config::TurnConfig;

const REALM: &str = "promtuz";
/// How long minted credentials stay good. Long enough for any call to be
/// set up on them; a call already running keeps its allocation regardless.
const CREDENTIAL_LIFETIME: Duration = Duration::from_secs(60 * 60);

pub struct Turn {
    pub port: u16,
    public_ip: IpAddr,
    secret: hmac::Key,
    socket: Mutex<Option<std::net::UdpSocket>>,
}

impl std::fmt::Debug for Turn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Turn").field("port", &self.port).field("public_ip", &self.public_ip).finish()
    }
}

impl Turn {
    /// Bind the TURN port now, so a taken port fails at startup, and keep the
    /// socket for [`Self::serve`].
    pub fn bind(cfg: &TurnConfig, signing: &SigningKey) -> Result<Self> {
        let public_ip = cfg.public_ip.context("[turn] public_ip is required when enabled")?;
        let socket = std::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], cfg.port)))
            .with_context(|| format!("binding the TURN socket on port {}", cfg.port))?;
        socket.set_nonblocking(true)?;
        let root = hmac::Key::new(hmac::HMAC_SHA256, &signing.to_bytes());
        let secret =
            hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&root, b"promtuz turn secret v1").as_ref());
        Ok(Self { port: cfg.port, public_ip, secret, socket: Mutex::new(Some(socket)) })
    }

    /// Credentials for `client`, valid for [`CREDENTIAL_LIFETIME`] from `now_ms`.
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

    /// Run the server until `cancel`. Takes the bound socket; a second call
    /// finds none and returns.
    pub async fn serve(self: Arc<Self>, cancel: CancellationToken) {
        let Some(std_sock) = self.socket.lock().take() else { return };
        let sock = match tokio::net::UdpSocket::from_std(std_sock) {
            Ok(s) => s,
            Err(e) => return warn!("TURN socket unusable: {e}"),
        };
        let config = ServerConfig {
            conn_configs: vec![ConnConfig {
                conn: Arc::new(sock),
                relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                    relay_address: self.public_ip,
                    address: "0.0.0.0".into(),
                    net: Arc::new(Net::new(None)),
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
    /// The key TURN expects for `username`, if the credential is one we
    /// minted and it has not expired. A wrong password fails the message
    /// integrity check inside the TURN server rather than here.
    fn auth_handle(&self, username: &str, realm: &str, _src: SocketAddr) -> Result<Vec<u8>, turn::Error> {
        let expiry: u64 = username
            .split(':')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| turn::Error::Other("malformed TURN username".into()))?;
        if expiry < crate::util::systime().as_secs() {
            return Err(turn::Error::Other("expired TURN credentials".into()));
        }
        Ok(generate_auth_key(username, realm, &self.password_for(username)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn() -> Turn {
        let cfg = TurnConfig { enabled: true, port: 0, public_ip: Some("203.0.113.9".parse().unwrap()) };
        Turn::bind(&cfg, &common::crypto::get_signing_key()).unwrap()
    }

    #[test]
    fn minted_credentials_authenticate_until_they_expire() {
        let t = turn();
        let now = crate::util::systime().as_millis() as u64;
        let creds = t.credentials(&[7u8; 32], now);
        let expected = generate_auth_key(&creds.username, REALM, &creds.password);
        let src = "10.0.0.1:1".parse().unwrap();
        assert_eq!(t.auth_handle(&creds.username, REALM, src).unwrap(), expected);

        let stale = t.credentials(&[7u8; 32], now - 2 * CREDENTIAL_LIFETIME.as_millis() as u64);
        assert!(t.auth_handle(&stale.username, REALM, src).is_err(), "expired must be refused");
        assert!(t.auth_handle("garbage", REALM, src).is_err());
    }

    /// The secret comes from the relay key, so two relays never honour each
    /// other's credentials and a restart honours its own.
    #[test]
    fn passwords_are_bound_to_the_relay_key() {
        let cfg = TurnConfig { enabled: true, port: 0, public_ip: Some("203.0.113.9".parse().unwrap()) };
        let key = common::crypto::get_signing_key();
        let a = Turn::bind(&cfg, &key).unwrap();
        let again = Turn::bind(&cfg, &key).unwrap();
        let other = Turn::bind(&cfg, &common::crypto::get_signing_key()).unwrap();
        let creds = a.credentials(&[1u8; 32], 1_700_000_000_000);
        assert_eq!(again.password_for(&creds.username), creds.password);
        assert_ne!(other.password_for(&creds.username), creds.password);
    }
}
