use common::msg::cbor::{FromCbor, ToCbor};
use common::msg::client::ClientRequest;

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

            println!("CLIENT: OPENED BI STREAM");

            let resolver = resolver.clone();
            tokio::spawn(async move {
                let data = recv.read_to_end(64 * 1024).await.ok()?;

                println!("CLIENT: DATA({:?})", data);
                
                let req = ClientRequest::from_cbor(&data).ok()?;

                println!("CLIENT: REQUEST({:?})", req);

                let res = resolver.lock().await.handle_rpc(req).await.ok()?;

                println!("CLIENT: RESPONSE({:?})", res);

                let encoded = res.to_cbor().ok()?;

                send.write_all(&encoded).await.ok()?;
                send.finish().ok()?;

                Some(())
            });
        }

        println!("CLIENT_CLOSE({}): {}", self.conn.remote_address(), self.conn.closed().await);
    }
}
