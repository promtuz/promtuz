use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;
use common::msg::client::ClientRequest;
use tokio::io::AsyncReadExt;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use crate::resolver::rpc::HandleRPC;

pub trait HandleClient {
    async fn handle_client(self, resolver: ResolverRef);
}

impl HandleClient for Handler {
    async fn handle_client(self, resolver: ResolverRef) {
        let conn = self.conn.clone();

        println!("CLIENT: CONN({})", self.conn.remote_address());

        loop {
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                break;
            };

            let resolver = resolver.clone();

            tokio::spawn(async move {
                while let Ok(packet_size) = recv.read_u32().await {
                    println!("CLIENT: PACKET({})", packet_size);
                    let mut packet = vec![0u8; packet_size as usize];

                    if let Err(err) = recv.read_exact(&mut packet).await {
                        println!("Read failed : {}", err); // temp
                        break;
                    }

                    println!("CLIENT: PACKET({})", hex::encode(&packet));

                    let req = ClientRequest::from_cbor(&packet).ok()?;

                    let res = resolver.lock().await.handle_rpc(req).await.ok()?;

                    let packet = res.pack().ok()?;

                    send.write_all(&packet).await.ok()?;
                    send.finish().ok()?;
                }

                Some(())
            });
        }

        println!("CLIENT_CLOSE({}): {}", self.conn.remote_address(), self.conn.closed().await);
    }
}
