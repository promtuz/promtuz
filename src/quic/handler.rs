use std::sync::Arc;

use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::resolver::*;
use common::quic::protorole::ProtoRole;
use common::quic::send_uni;
use quinn::Connection;

use crate::resolver::ResolverRef;
use crate::ret;

pub struct Handler {
    conn: Arc<Connection>,
}

impl Handler {
    pub async fn handle(conn: Connection, resolver: ResolverRef) {
        let role = ret!(ProtoRole::from_conn(&conn));

        let handler = Self { conn: Arc::new(conn) };

        match role {
            ProtoRole::Resolver => {
                todo!("SUPPORT: resolver/1")
            },
            ProtoRole::Client => {
                todo!("SUPPORT: client/1")
            },
            ProtoRole::Relay => handler.handle_relay(resolver).await,
            _ => handler.conn.close(0u32.into(), b"UnsupportedALPN"),
        };
    }

    async fn handle_relay(self, resolver: ResolverRef) {
        while let Ok(mut recv) = self.conn.accept_uni().await {
            let conn = self.conn.clone();
            let resolver = resolver.clone();

            tokio::spawn(async move {
                loop {
                    let conn = conn.clone();
                    match recv.read_chunk(4096, true).await {
                        Ok(Some(chunk)) => {
                            let bytes = chunk.bytes;
                            if let Ok(hello) = RelayHello::from_cbor(&bytes) {
                                let hello_ack = match resolver
                                    .lock()
                                    .await
                                    .register_relay(conn.clone(), &hello)
                                {
                                    Ok(ack) => ret!(ack.to_cbor().ok()),
                                    Err(close) => {
                                        return conn.close(close.code(), &close.reason());
                                    },
                                };

                                send_uni(&conn, &hello_ack).await.ok();
                            } else if let Ok(_hb) = RelayHeartbeat::from_cbor(&bytes) {
                                println!("RELAY_HB");
                            } else {
                                println!("RELAY_PACKET: {}", String::from_utf8_lossy(&bytes));
                            }
                        },
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            });
        }
    }
}
