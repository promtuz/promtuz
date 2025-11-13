use anyhow::Result;
use quinn::Connection;

pub mod id;
pub mod config;
pub mod protorole;



pub async fn send_uni(conn: &Connection, data: &[u8]) -> Result<()> {
    let mut send = conn.open_uni().await?;
    send.write_all(data).await?;
    send.finish()?;

    Ok(())
}
