use std::future::IntoFuture;
use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use common::debug;
use common::warn;
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use quinn::Endpoint;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::quic::handler::Handler;
use crate::relay::RelayRef;

/// Maximum time we wait for in-flight per-connection tasks to wind down on
/// shutdown. After this, surviving tasks are aborted so the process can
/// actually exit. Five seconds matches the resolver's `wait_idle` budget.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Sustained accepted-connections-per-source-IP, per minute. A loose ceiling
/// rather than a real quota: an entire carrier-NAT'd population shares one
/// egress address here, so this only has to stop a trivial flood without
/// denying a whole mobile network. [`MAX_LIVE_CONNECTIONS`] is what actually
/// bounds concurrency.
const ACCEPT_RATE_PER_MIN: u32 = 600;

/// Short-term burst allowed above the sustained rate (reconnect headroom — a
/// carrier NAT reconnecting after an outage arrives all at once).
const ACCEPT_RATE_BURST: u32 = 300;

/// Ceiling on per-connection handler tasks alive at once, across all sources.
const MAX_LIVE_CONNECTIONS: usize = 8192;

/// Budget for the QUIC/TLS handshake, before any role handler runs. Bounds
/// how long a permit can be held by a peer that never finishes connecting.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

type IpRateLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

/// Accepts all incoming connections for given endpoint and handles them accordingly
pub struct Acceptor {
    /// Clone of endpoint reference from [Relay]
    endpoint: Endpoint,
    /// The default keyed in-memory store evicts idle IPs automatically, so
    /// this does not grow unboundedly under churn.
    limiter: Arc<IpRateLimiter>,
    /// One permit per live handler task; the permit is moved into the task
    /// and released when it returns.
    slots:   Arc<Semaphore>,
}

impl Acceptor {
    pub fn new(endpoint: Endpoint) -> Self {
        // Non-zero compile-time literals; `or(MIN)` is a defensive fallback if
        // someone later edits a constant to zero.
        let per_minute = NonZeroU32::new(ACCEPT_RATE_PER_MIN).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(ACCEPT_RATE_BURST).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::per_minute(per_minute).allow_burst(burst);

        Self {
            endpoint,
            limiter: Arc::new(RateLimiter::keyed(quota)),
            slots: Arc::new(Semaphore::new(MAX_LIVE_CONNECTIONS)),
        }
    }

    /// Run the accept loop. Per-connection handlers are tracked in a
    /// `JoinSet` so shutdown can cooperatively await them; on `cancel`,
    /// stop accepting new connections, then wait up to `SHUTDOWN_GRACE`
    /// before aborting whatever's left.
    pub async fn run(&self, relay: RelayRef, cancel: CancellationToken) {
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            while tasks.try_join_next().is_some() {}

            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!("acceptor: shutdown signal received, draining {} task(s)", tasks.len());
                    break;
                }
                incoming = self.endpoint.accept() => {
                    let Some(conn) = incoming else { break; };

                    // `refuse()` rejects before the handshake, so neither
                    // rejection path costs us crypto or a task.
                    let ip = conn.remote_address().ip();
                    if self.limiter.check_key(&ip).is_err() {
                        debug!("refusing conn from {ip}: per-IP rate limit exceeded");
                        conn.refuse();
                        continue;
                    }
                    let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                        debug!("refusing conn from {ip}: at the {MAX_LIVE_CONNECTIONS}-connection cap");
                        conn.refuse();
                        continue;
                    };

                    let relay = relay.clone();
                    let cancel_child = cancel.clone();
                    tasks.spawn(async move {
                        let _permit = permit;
                        let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.into_future());
                        if let Ok(Ok(connection)) = handshake.await {
                            Handler::handle(connection, relay, cancel_child).await;
                        }
                    });
                }
                _ = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }

        // Cooperative drain — handlers that observe `cancel` will return
        // promptly. Anything stuck (e.g. blocked on a syscall, or a packet
        // handler that didn't propagate the token) gets aborted.
        match tokio::time::timeout(SHUTDOWN_GRACE, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        {
            Ok(()) => debug!("acceptor: all connection tasks drained cleanly"),
            Err(_) => {
                warn!(
                    "acceptor: {} task(s) still running after {:?}, aborting",
                    tasks.len(),
                    SHUTDOWN_GRACE
                );
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            },
        }
    }
}
