use std::sync::Arc;

use common::debug;
use common::proto::client_res::ClientRequest;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::warn;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use crate::resolver::rpc::HandleRPC;

pub trait HandleClient {
    async fn handle_client(self, resolver: ResolverRef);
}

impl HandleClient for Handler {
    async fn handle_client(self, resolver: ResolverRef) {
        debug!("incoming client({}) conn", self.conn.remote_address());
        serve_rpc_streams(self.conn.clone(), resolver).await;
    }
}

/// Serve the **one-RPC-per-bi-stream** contract on `conn`: each accepted
/// bi-stream is exactly one [`ClientRequest`] → [`HandleRPC::handle_rpc`] →
/// one response, then the stream closes. This keeps state simple and makes
/// concurrency a per-stream property of QUIC itself, avoiding the
/// half-closed-stream foot-gun the previous loop suffered from.
///
/// Shared by [`HandleClient::handle_client`] (client connections) **and**
/// the relay connection handler: a relay issues read-only registry RPCs
/// over its *existing* resolver session — notably the DHT
/// `GetBootstrapPeers` bootstrap query (`relay/src/quic/resolver_link.rs`)
/// — so the same bi-stream service must run there too, alongside the uni
/// lifecycle loop. Without it those RPCs open a stream the resolver never
/// accepts, and the relay blocks forever waiting for a reply.
pub(super) async fn serve_rpc_streams(conn: Arc<Connection>, resolver: ResolverRef) {
    let addr = conn.remote_address();

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };

        let resolver = resolver.clone();

        tokio::spawn(async move {
            // 1. read one request
            let req = match ClientRequest::unpack(&mut recv).await {
                Ok(req) => req,
                Err(e) => {
                    warn!("client({addr}) request decode failed: {e}");
                    return;
                },
            };

            // 2. dispatch (no lock — Resolver is Arc<Resolver>)
            let res = match resolver.handle_rpc(req).await {
                Ok(res) => res,
                Err(e) => {
                    warn!("client({addr}) rpc handler failed: {e}");
                    return;
                },
            };

            // 3. encode + write + finish, exactly once
            let packet = match res.pack() {
                Ok(p) => p,
                Err(e) => {
                    warn!("client({addr}) response encode failed: {e}");
                    return;
                },
            };

            if let Err(e) = send.write_all(&packet).await {
                warn!("client({addr}) response write failed: {e}");
                return;
            }
            if let Err(e) = send.finish() {
                warn!("client({addr}) stream finish failed: {e}");
            }
        });
    }
}
