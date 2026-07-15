use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use common::debug;
use common::quic::CloseReason;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use crate::gateway::Gateway;
use crate::quic::handler::Handler;

/// Sustained accepted-connections-per-source-IP, per minute. A device
/// registers once per install and a home relay connects rarely, so this only
/// bites a probe flood.
const ACCEPT_RATE_PER_MIN: u32 = 10;

/// Short-term burst allowed above the sustained rate (reconnect headroom).
const ACCEPT_RATE_BURST: u32 = 5;

type IpRateLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Accepts inbound connections and hands each to [`Handler`], rate-limiting
/// per source IP before spending CPU on the handshake.
pub struct Acceptor {
    gateway: Arc<Gateway>,
    /// The default keyed in-memory store evicts idle IPs automatically, so
    /// this does not grow unboundedly under churn.
    limiter: Arc<IpRateLimiter>,
}

impl Acceptor {
    pub fn new(gateway: Arc<Gateway>) -> Self {
        // Non-zero compile-time literals; `or(MIN)` is a defensive fallback if
        // someone later edits a constant to zero.
        let per_minute = NonZeroU32::new(ACCEPT_RATE_PER_MIN).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(ACCEPT_RATE_BURST).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::per_minute(per_minute).allow_burst(burst);
        Self { gateway, limiter: Arc::new(RateLimiter::keyed(quota)) }
    }

    pub async fn run(&self) {
        while let Some(conn) = self.gateway.endpoint.accept().await {
            let limiter = self.limiter.clone();
            let gateway = self.gateway.clone();
            tokio::spawn(async move {
                // Rate-limit on the source IP (visible from the QUIC Initial)
                // before doing crypto for a potential flooder.
                let ip = conn.remote_address().ip();
                if limiter.check_key(&ip).is_err() {
                    debug!("rejecting conn from {ip}: per-IP rate limit exceeded");
                    if let Ok(connection) = conn.await {
                        CloseReason::RateLimited.close(&connection);
                    }
                    return;
                }

                if let Ok(connection) = conn.await {
                    Handler::handle(connection, gateway).await;
                }
            });
        }
    }
}
