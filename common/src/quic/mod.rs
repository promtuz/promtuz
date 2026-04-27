use anyhow::Result;
use quinn::Connection;
use quinn::VarInt;

pub mod config;
pub mod id;
#[cfg(feature = "server")]
pub mod p256;
pub mod protorole;

/// Heartbeat interval in seconds
pub static RESOLVER_RELAY_HEARTBEAT_INTERVAL: u64 = 20;

pub async fn send_uni(conn: &Connection, data: &[u8]) -> Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(data).await?;
    send.finish()?;

    Ok(())
}

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
pub enum CloseReason {
    DuplicateConnect,
    AlreadyConnected,
    ShuttingDown,
    Reconnecting,
    PacketMismatch,
    /// Resolver: incoming `RelayHello` failed signature/identity validation
    /// (id-to-key mismatch, malformed pubkey, or bad Ed25519 sig).
    BadSignature,
    /// Resolver: `RelayHello.timestamp` is outside the accepted clock window.
    StaleTimestamp,
    /// Resolver: registry is at capacity, no more relays can be admitted
    /// until existing ones disconnect.
    RegistryFull,
    /// Peer ALPN-negotiated a protocol role (e.g. `resolver/1`) for which
    /// this side has no implementation. Closing politely is preferable to
    /// panicking the spawned per-connection task.
    UnsupportedRole,
    /// Source address has exceeded its accept-side rate-limit quota.
    /// Returned at the acceptor before any per-connection state is created.
    RateLimited,
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
