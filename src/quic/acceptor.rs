use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::resolver::HelloAck;
use common::msg::resolver::RelayHeartbeat;
use common::msg::resolver::RelayHello;
use common::quic::protorole::ProtoRole;
use common::quic::send_uni;
use quinn::Connection;
use quinn::Endpoint;

use crate::util::systime_sec;

pub async fn run_acceptor(ep: Arc<Endpoint>) {
    while let Some(conn) = ep.accept().await {
        tokio::spawn(async move {
            if let Ok(connection) = conn.await
                && let Err(err) = handle_connection(Arc::new(connection)).await
            {
                println!("CONN_ERR: {}", err)
            }
        });
    }
}

pub async fn handle_connection(conn: Arc<Connection>) -> Result<()> {
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

pub async fn handle_relay(conn: Arc<Connection>) -> Result<()> {
    while let Ok(mut recv) = conn.accept_uni().await {
        let conn = conn.clone();
        tokio::spawn(async move {
            loop {
                match recv.read_chunk(4096, true).await {
                    Ok(Some(chunk)) => {
                        let bytes = chunk.bytes;
                        if let Ok(hello) = RelayHello::from_cbor(&bytes) {
                            println!("RELAY_HELLO: {:?}", hello);
                            let jitter = (rand::random::<f32>() * 2000.0 - 1000.0) as i32;
                            let hello_ack = HelloAck {
                                accepted: true,
                                interval_heartbeat_ms: (25 * 1000 + jitter).max(0) as u32,
                                reason: None,
                                resolver_time: systime_sec(),
                            };

                            let hello_ack = match hello_ack.to_cbor() {
                                Ok(x) => x,
                                Err(_) => return,
                            };

                            send_uni(&conn, &hello_ack).await.ok();
                        } else if let Ok(hb) = RelayHeartbeat::from_cbor(&bytes) {
                            println!("RELAY_HB: {:?}", hb);
                        } else {
                            println!("RELAY_PACKET: {}", String::from_utf8_lossy(&bytes));
                        }
                    },
                    Ok(None) => break, // EOF
                    Err(_) => break,
                }
            }
        });
    }

    Ok(())
}
