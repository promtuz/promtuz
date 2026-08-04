use std::num::NonZeroU32;
use std::sync::Arc;

use common::debug;
use common::proto::client_res::ClientRequest;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use common::warn;
use governor::DefaultDirectRateLimiter;
use governor::Quota;
use governor::RateLimiter;
use quinn::Connection;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;
use crate::resolver::rpc::HandleRPC;

/// Sustained RPCs one connection may issue per minute. A client refreshes
/// its relay list rarely and a relay bootstraps once per session, so this
/// only bites a peer that holds a session open to grind responses out of us.
const RPC_RATE_PER_MIN: u32 = 60;

/// Short-term burst allowed above the sustained RPC rate.
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

fn rpc_limiter() -> DefaultDirectRateLimiter {
    let per_minute = NonZeroU32::new(RPC_RATE_PER_MIN).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(RPC_RATE_BURST).unwrap_or(NonZeroU32::MIN);
    RateLimiter::direct(Quota::per_minute(per_minute).allow_burst(burst))
}

/// Serve the **one-RPC-per-bi-stream** contract on `conn`: each accepted
/// bi-stream is exactly one [`ClientRequest`] → [`HandleRPC::handle_rpc`] →
/// one response, then the stream closes. This keeps state simple and makes
/// concurrency a per-stream property of QUIC itself, avoiding the
/// half-closed-stream foot-gun the previous loop suffered from.
///
/// Metered per connection at stream accept — one token per request. The
/// acceptor's per-IP quota only meters new connections, so this is the sole
/// bound on what a peer can extract from a session it already holds.
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
    let limiter = rpc_limiter();

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
            // 1. read one request
            let req = match ClientRequest::unpack(&mut recv).await {
                Ok(req) => req,
                Err(e) => {
                    warn!("client({addr}) request decode failed: {e}");
                    return;
                },
            };

            // 2. dispatch (no lock — Resolver is Arc<Resolver>)
            let packet = match resolver.handle_rpc(req).await {
                Ok(packet) => packet,
                Err(e) => {
                    warn!("client({addr}) rpc handler failed: {e}");
                    return;
                },
            };

            // 3. write + finish, exactly once
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
