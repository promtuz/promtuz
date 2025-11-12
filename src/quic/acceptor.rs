use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::msg::cbor::FromCbor;
use common::msg::resolver::RelayHello;
use common::quic::protorole::ProtoRole;
use quinn::Connection;
use quinn::Endpoint;

pub async fn run_acceptor(ep: Arc<Endpoint>) {
    while let Some(conn) = ep.accept().await {
        tokio::spawn(async move {
            if let Ok(connection) = conn.await
                && let Err(err) = handle_connection(connection).await
            {
                println!("CONN_ERR: {}", err)
            }
        });
    }
}

pub async fn handle_connection(conn: Connection) -> Result<()> {
    let role = ProtoRole::from_conn(&conn).ok_or(anyhow!("Unsupported ALPN"))?;

    match role {
        ProtoRole::Resolver => {
            todo!("SUPPORT: resolver/1")
        },
        ProtoRole::Client => {
            todo!("SUPPORT: client/1")
        },
        ProtoRole::Relay => handle_relay(conn).await?,
        _ => conn.close(0u32.into(), b"UnsupportedALPN"),
    };

    Ok(())
}

pub async fn handle_relay(conn: Connection) -> Result<()> {
    loop {
        let (mut _tx, mut rx) = conn.accept_bi().await?;

        if let Ok(packet) = rx.read_to_end(4096).await {
            if let Ok(hello) = RelayHello::from_cbor(&packet) {
                println!("RELAY_HELLO: {:?}", hello)
            } else {
                println!("RELAY_PACKET: {}", String::from_utf8_lossy(&packet));
            }
        };
    }
}
