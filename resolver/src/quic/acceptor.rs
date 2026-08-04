use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use common::debug;
use common::quic::CloseReason;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use quinn::Endpoint;

use crate::quic::handler::Handler;
use crate::resolver::ResolverRef;

/// Sustained rate of accepted connections per source IP, in
/// connections-per-minute. Picked low enough that a typical legitimate
/// client (which dials at most a handful of times per session) is never
/// touched, while a probe-flood attacker is throttled almost immediately.
const ACCEPT_RATE_PER_MIN: u32 = 10;

/// Allowed short-term burst above the sustained rate. Five gives a brand-new
/// peer enough headroom for a quick reconnect storm (e.g. after a
/// transient network blip) without classifying the burst as abuse.
const ACCEPT_RATE_BURST: u32 = 5;

/// Housekeeping interval for the keyed limiter on an otherwise idle endpoint.
const LIMITER_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Accepts between housekeeping passes, so a flood of distinct source
/// addresses cannot outrun the timer-driven sweep.
const SWEEP_EVERY_ACCEPTS: usize = 4096;

type IpRateLimiter =
    RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Accepts all incoming connections for given endpoint and handles them accordingly
pub struct Acceptor {
    /// Clone of endpoint reference from [Resolver]
    endpoint: Arc<Endpoint>,
    /// Per-source-IP accept-side rate limiter. Keys are the pre-handshake
    /// source address, so the store only stays bounded because
    /// [`sweep_limiter`] and the accept-count sweep drop replenished buckets.
    limiter: Arc<IpRateLimiter>,
}

impl Acceptor {
    pub fn new(endpoint: Arc<Endpoint>) -> Self {
        // Both rate constants are non-zero compile-time literals; we use
        // `or(MIN)` purely as a defensive fallback in case someone later
        // edits them to zero. `NonZeroU32::MIN` (== 1) is itself a const,
        // so this entire expression has zero runtime cost.
        let per_minute = NonZeroU32::new(ACCEPT_RATE_PER_MIN).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(ACCEPT_RATE_BURST).unwrap_or(NonZeroU32::MIN);

        let quota = Quota::per_minute(per_minute).allow_burst(burst);
        let limiter = Arc::new(RateLimiter::keyed(quota));

        Self { endpoint, limiter }
    }

    pub async fn run(&self, resolver: ResolverRef) {
        tokio::spawn(sweep_limiter(self.limiter.clone()));

        let mut since_sweep = 0usize;
        while let Some(conn) = self.endpoint.accept().await {
            since_sweep += 1;
            if since_sweep >= SWEEP_EVERY_ACCEPTS {
                since_sweep = 0;
                self.limiter.retain_recent();
                self.limiter.shrink_to_fit();
            }

            let resolver = resolver.clone();
            let limiter = self.limiter.clone();

            tokio::spawn(async move {
                // Rate-limit *before* spending CPU on the QUIC handshake so
                // a flooder never gets us to do crypto for them. We can
                // already see the source IP at this point (the QUIC Initial
                // packet has been received).
                let ip = conn.remote_address().ip();
                if limiter.check_key(&ip).is_err() {
                    debug!("rejecting conn from {ip}: per-IP rate limit exceeded");
                    // Reject without doing the full handshake. We have to
                    // await `conn` to get a `Connection` we can `close()`
                    // on; the alternative would be `Connecting::ignore()`
                    // which silently drops without informing the peer.
                    if let Ok(connection) = conn.await {
                        CloseReason::RateLimited.close(&connection);
                    }
                    return;
                }

                if let Ok(connection) = conn.await {
                    Handler::handle(connection, resolver).await;
                }
            });
        }
    }
}

async fn sweep_limiter(limiter: Arc<IpRateLimiter>) {
    let mut ticker = tokio::time::interval(LIMITER_SWEEP_INTERVAL);
    loop {
        ticker.tick().await;
        limiter.retain_recent();
        limiter.shrink_to_fit();
    }
}
