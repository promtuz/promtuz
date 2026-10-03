//! Connection admission for the daemons' QUIC endpoints and the TLS fallback listener.

use std::future::Future;
use std::future::IntoFuture;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use quinn::Connection;
use quinn::Endpoint;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// Accepts per minute and burst per [`source_group`], and the cap on live connections.
pub struct Policy {
    pub per_minute: u32,
    pub burst:      u32,
    pub max_live:   usize,
}

/// Bounds how long a peer that never finishes the TLS handshake holds a slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// IPv4 counts per address; IPv6 counts per /64, because a single host is routinely handed a
/// whole /64 and could otherwise present an unlimited supply of distinct addresses. A dual-stack
/// socket reports IPv4 peers as v4-mapped, which counts as the IPv4 address.
pub fn source_group(ip: IpAddr) -> [u8; 16] {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => {
            let [a, b, c, d, ..] = v6.segments();
            Ipv6Addr::new(a, b, c, d, 0, 0, 0, 0).octets()
        },
    }
}

pub fn quota(per_minute: u32, burst: u32) -> Quota {
    let per_minute = NonZeroU32::new(per_minute).unwrap_or(NonZeroU32::MIN);
    let burst = NonZeroU32::new(burst).unwrap_or(NonZeroU32::MIN);
    Quota::per_minute(per_minute).allow_burst(burst)
}

pub(crate) struct Gate {
    limiter: RateLimiter<[u8; 16], DefaultKeyedStateStore<[u8; 16]>, DefaultClock>,
    slots:   Arc<Semaphore>,
}

impl Gate {
    pub(crate) fn new(policy: &Policy) -> Self {
        Self {
            limiter: RateLimiter::keyed(quota(policy.per_minute, policy.burst)),
            slots:   Arc::new(Semaphore::new(policy.max_live)),
        }
    }

    pub(crate) fn admit(&self, ip: IpAddr) -> Result<OwnedSemaphorePermit, &'static str> {
        self.limiter.check_key(&source_group(ip)).map_err(|_| "rate limited")?;
        self.slots.clone().try_acquire_owned().map_err(|_| "at the live-connection cap")
    }

    /// governor never evicts on its own; unswept, the map keeps every source it has seen.
    pub(crate) fn sweep(&self) {
        self.limiter.retain_recent();
        self.limiter.shrink_to_fit();
    }
}

/// Refusal happens before the handshake, so a refused peer costs neither crypto nor a task.
/// Returns once the endpoint closes, after draining the connection tasks.
pub async fn serve<F, Fut>(endpoint: Endpoint, policy: Policy, handle: F)
where
    F: Fn(Connection) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let gate = Gate::new(&policy);
    let handle = Arc::new(handle);
    let mut tasks = JoinSet::new();
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    loop {
        tokio::select! {
            biased;
            _ = sweep.tick() => gate.sweep(),
            _ = tasks.join_next(), if !tasks.is_empty() => {},
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let ip = incoming.remote_address().ip();
                let permit = match gate.admit(ip) {
                    Ok(permit) => permit,
                    Err(why) => {
                        crate::debug!("refusing conn from {ip}: {why}");
                        incoming.refuse();
                        continue;
                    },
                };
                let handle = handle.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming.into_future());
                    if let Ok(Ok(conn)) = handshake.await {
                        handle(conn).await;
                    }
                });
            },
        }
    }
    drain(tasks).await;
}

pub(crate) async fn drain(mut tasks: JoinSet<()>) {
    let drained = tokio::time::timeout(SHUTDOWN_GRACE, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        let left = tasks.len();
        crate::warn!("{left} connection task(s) still running after {SHUTDOWN_GRACE:?}; aborting");
        tasks.shutdown().await;
    }
}
