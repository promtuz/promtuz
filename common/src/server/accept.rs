//! Connection admission for the daemons' QUIC endpoints and the TLS fallback listener.

use std::collections::HashMap;
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

/// Accepts per minute and burst per [`source_group`], and the caps on live connections in all and
/// per source group, so one source cannot hold every slot.
pub struct Policy {
    pub per_minute:          u32,
    pub burst:               u32,
    pub max_live:            usize,
    pub max_live_per_source: usize,
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
    limiter:    RateLimiter<[u8; 16], DefaultKeyedStateStore<[u8; 16]>, DefaultClock>,
    slots:      Arc<Semaphore>,
    sources:    HashMap<[u8; 16], Arc<Semaphore>>,
    per_source: usize,
}

impl Gate {
    pub(crate) fn new(policy: &Policy) -> Self {
        Self {
            limiter:    RateLimiter::keyed(quota(policy.per_minute, policy.burst)),
            slots:      Arc::new(Semaphore::new(policy.max_live)),
            sources:    HashMap::new(),
            per_source: policy.max_live_per_source,
        }
    }

    /// The connection holds both permits until it ends.
    pub(crate) fn admit(&mut self, ip: IpAddr) -> Result<[OwnedSemaphorePermit; 2], &'static str> {
        let group = source_group(ip);
        self.limiter.check_key(&group).map_err(|_| "rate limited")?;
        let per_source = self.per_source;
        let source =
            self.sources.entry(group).or_insert_with(|| Arc::new(Semaphore::new(per_source)));
        let source = source.clone().try_acquire_owned().map_err(|_| "at the per-source cap")?;
        let slot =
            self.slots.clone().try_acquire_owned().map_err(|_| "at the live-connection cap")?;
        Ok([source, slot])
    }

    /// Neither map forgets a source on its own; unswept, both keep every source they have seen.
    pub(crate) fn sweep(&mut self) {
        self.limiter.retain_recent();
        self.limiter.shrink_to_fit();
        let per_source = self.per_source;
        self.sources.retain(|_, source| source.available_permits() < per_source);
    }
}

/// Refusal happens before the handshake, so a refused peer costs neither crypto nor a task.
/// Returns once the endpoint closes, after draining the connection tasks.
pub async fn serve<F, Fut>(endpoint: Endpoint, policy: Policy, handle: F)
where
    F: Fn(Connection) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut gate = Gate::new(&policy);
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

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    /// One source group fills only its share of the live slots, another still gets in, and a
    /// closed connection frees its slot.
    #[test]
    fn one_source_cannot_hold_every_live_slot() {
        let policy =
            Policy { per_minute: 600, burst: 300, max_live: 300, max_live_per_source: 256 };
        let mut gate = Gate::new(&policy);
        let ip = |last| IpAddr::V4(Ipv4Addr::new(10, 0, 0, last));
        let mut held: Vec<_> = (0..256).map(|_| gate.admit(ip(1)).unwrap()).collect();
        assert_eq!(gate.admit(ip(1)).err(), Some("at the per-source cap"));
        let other: Vec<_> = (0..44).map(|_| gate.admit(ip(2)).unwrap()).collect();
        assert_eq!(gate.admit(ip(3)).err(), Some("at the live-connection cap"));
        drop(other);
        held.pop();
        assert!(gate.admit(ip(1)).is_ok());
        gate.sweep();
        assert_eq!(gate.sources.len(), 1, "only a source with live connections is kept");
    }
}
