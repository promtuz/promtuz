use anyhow::Result;
use quinn::Connection;
use quinn::VarInt;

pub mod config;
pub mod id;
#[cfg(feature = "server")]
pub mod p256;
pub mod protorole;
pub mod xor;
#[cfg(feature = "crypto")]
pub mod tunnel;
#[cfg(all(feature = "crypto", feature = "server"))]
pub mod tunnel_listener;

pub use xor::xor32;

/// Heartbeat interval in seconds
pub static RESOLVER_RELAY_HEARTBEAT_INTERVAL: u64 = 20;

/// TLS exporter output both ends compute under `label`. A proof bound to it verifies on this
/// connection and no other.
pub fn session_binding(conn: &Connection, label: &[u8]) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(&mut out, label, &[])
        .map_err(|e| anyhow::anyhow!("tls exporter: {e:?}"))?;
    Ok(out)
}

#[cfg(feature = "proto")]
pub fn client_auth_binding(conn: &Connection) -> Result<[u8; 32]> {
    session_binding(conn, crate::proto::client_rel::CLIENT_AUTH_EXPORTER_LABEL)
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CloseReason {
    DuplicateConnect,
    AlreadyConnected,
    ShuttingDown,
    Reconnecting,
    PacketMismatch,
    BadSignature,
    StaleTimestamp,
    RegistryFull,
    UnsupportedRole,
    RateLimited,
    DhtBadSignature,
    DhtClockSkew,
    DhtNotOwner,
    DhtFlood,
    /// Any malformed DHT frame or key.
    DhtMalformedKey,
    DhtForwardRejected,
    KeyPackageMalformed,
    KeyPackageExpired,
    KeyPackageRateLimited,
    WelcomeMalformed,
    WelcomeQueueFull,
    WelcomeRateLimited,
}

impl CloseReason {
    pub fn reason(&self) -> Vec<u8> {
        format!("{:?}", self).into()
    }
    pub fn code(&self) -> VarInt {
        VarInt::from_u32(*self as u32 + 1)
    }

    pub fn close(self, conn: &Connection) {
        conn.close(self.code(), &self.reason());
    }
}
