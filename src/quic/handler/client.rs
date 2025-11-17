use common::msg::cbor::FromCbor;
use common::msg::cbor::ToCbor;

use crate::proto::client::ClientRequest;
use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use crate::resolver::rpc::HandleRPC;

pub trait HandleClient {
    async fn handle_client(self, resolver: ResolverRef);
}

impl HandleClient for Handler {
    async fn handle_client(self, resolver: ResolverRef) {
        let conn = self.conn.clone();

        loop {
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                break;
            };

            let resolver = resolver.clone();
            tokio::spawn(async move {
                let data = recv.read_to_end(64 * 1024).await.ok()?;
                let req = ClientRequest::from_cbor(&data).ok()?;
                let res = resolver.lock().await.handle_rpc(req).await.ok()?;

                let encoded = res.to_cbor().ok()?;

                send.write_all(&encoded).await.ok()?;
                send.finish().ok()?;

                Some(())
            });
        }
    }
}
