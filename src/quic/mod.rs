use anyhow::Result;
use quinn::Connection;

pub mod config;
pub mod id;
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
