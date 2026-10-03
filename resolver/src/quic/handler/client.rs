use std::sync::Arc;

use common::debug;
use common::proto::client_res::ClientRequest;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use common::server::accept::quota;
use common::warn;
use governor::RateLimiter;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use crate::resolver::rpc::HandleRPC;

const RPC_RATE_PER_MIN: u32 = 60;

const RPC_RATE_BURST: u32 = 20;

pub trait HandleClient {
    async fn handle_client(self, resolver: ResolverRef);
}

impl HandleClient for Handler {
    async fn handle_client(self, resolver: ResolverRef) {
        debug!("incoming client({}) conn", self.conn.remote_address());
        serve_rpc_streams(self.conn.clone(), resolver).await;
    }
}

/// Relays issue RPCs on the same session, so the relay handler serves these streams too. The
/// acceptor meters only new connections, so this limiter is the sole bound on an open session.
pub(super) async fn serve_rpc_streams(conn: Arc<Connection>, resolver: ResolverRef) {
    let addr = conn.remote_address();
    let limiter = RateLimiter::direct(quota(RPC_RATE_PER_MIN, RPC_RATE_BURST));

    loop {
        let (mut send, mut recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => break,
        };

        if limiter.check().is_err() {
            debug!("client({addr}) rpc rate limit exceeded");
            CloseReason::RateLimited.close(&conn);
            break;
        }

        let resolver = resolver.clone();

        tokio::spawn(async move {
            let req = match ClientRequest::unpack(&mut recv).await {
                Ok(req) => req,
                Err(e) => {
                    warn!("client({addr}) request decode failed: {e}");
                    return;
                },
            };

            let packet = match resolver.handle_rpc(req).await {
                Ok(packet) => packet,
                Err(e) => {
                    warn!("client({addr}) rpc handler failed: {e}");
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
