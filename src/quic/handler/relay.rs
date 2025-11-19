use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::resolver::RelayHeartbeat;
use common::msg::resolver::RelayHello;
use common::quic::send_uni;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use common::ret;

pub(super) trait HandleRelay {
    async fn handle_relay(self, resolver: ResolverRef);
}

impl HandleRelay for Handler {
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
                                // println!("RELAY_HB");
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
