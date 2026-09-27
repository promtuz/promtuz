//! A route-owned, finite TCP race. Both peers must join the authenticated
//! bridge; its namespace never admits legacy UDP bearer registrations.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use common::node::config::DEFAULT_RESOLVER_PORT;
use common::proto::client_res::{ClientRequest, ClientResponse, RelayDescriptor};
use common::proto::pack::{Packer, Unpacker};
use common::quic::tunnel::{self, Request};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::time::Instant;

use super::diagnostics::{self, Event};
use super::socket::TurnRoutes;
use crate::data::identity::{Identity, IdentitySigner};
use crate::db::network::NETWORK_DB;

const HEAD_START: Duration = Duration::from_millis(600);
const JOIN_DEADLINE: Duration = Duration::from_secs(8);
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(3);
const DISCOVERY_COOLDOWN: Duration = Duration::from_secs(60);
const DISCOVERY_CONCURRENCY: usize = 4;
static DISCOVERY_CURSOR: AtomicUsize = AtomicUsize::new(0);
static DISCOVERY: Lazy<tokio::sync::Mutex<Option<Instant>>> =
    Lazy::new(|| tokio::sync::Mutex::new(None));

/// A peer may name a relay address, but never supplies the identity trusted by
/// TLS. Only our resolver-authenticated local relay records supply that name.
fn known_relay_name(relay: SocketAddr) -> Option<String> {
    let db = NETWORK_DB.lock();
    known_relay_name_in(&db, relay)
}

fn known_relay_name_in(db: &rusqlite::Connection, relay: SocketAddr) -> Option<String> {
    let mut statement = db.prepare("SELECT id, host FROM relays WHERE port = ?1").ok()?;
    let rows = statement
        .query_map([relay.port()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .ok()?;
    rows.filter_map(Result::ok).find_map(|(id, host)| {
        let address = SocketAddr::new(host.parse().ok()?, relay.port());
        (super::reflexive::canonical(address) == super::reflexive::canonical(relay)).then_some(id)
    })
}

/// Serialize and throttle unknown-relay refreshes. Concurrent sessions share
/// the new authenticated records; peer offers cannot cause unbounded resolver
/// traffic. The whole discovery, including waiting for another refresh, is
/// inside the route's separate setup deadline.
async fn refreshed_name<L, R, F>(
    state: &tokio::sync::Mutex<Option<Instant>>, lookup: L, refresh: R,
) -> Option<String>
where
    L: Fn() -> Option<String>,
    R: FnOnce() -> F,
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    if let Some(name) = lookup() {
        return Some(name);
    }
    let mut last = state.lock().await;
    if let Some(name) = lookup() {
        return Some(name);
    }
    if last.is_some_and(|at| at.elapsed() < DISCOVERY_COOLDOWN) {
        return None;
    }
    *last = Some(Instant::now());
    tokio::time::timeout(DISCOVERY_DEADLINE, refresh()).await.ok()?.ok()?;
    lookup()
}

/// Race a finite batch of complete lookups, not just connections. A resolver
/// that accepts TLS but never answers cannot hold up another trusted seed.
/// The rotating batch bounds work while giving every configured seed a turn.
/// Inline futures ensure a winner or caller cancellation drops every loser.
async fn race_seed_lookups<T, L, F>(
    count: usize, cursor: &AtomicUsize, lookup: L,
) -> anyhow::Result<T>
where
    L: Fn(usize) -> F,
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    anyhow::ensure!(count > 0, "no trusted resolver seeds");
    let batch = count.min(DISCOVERY_CONCURRENCY);
    let start = cursor.fetch_add(batch, Ordering::Relaxed) % count;
    let mut pending: Vec<_> =
        (0..batch).map(|offset| Some(Box::pin(lookup((start + offset) % count)))).collect();
    let mut last_error = None;
    std::future::poll_fn(|cx| {
        for slot in &mut pending {
            let Some(future) = slot.as_mut() else { continue };
            if let std::task::Poll::Ready(result) = future.as_mut().poll(cx) {
                *slot = None;
                match result {
                    Ok(value) => return std::task::Poll::Ready(Ok(value)),
                    Err(error) => last_error = Some(error),
                }
            }
        }
        if pending.iter().all(Option::is_none) {
            std::task::Poll::Ready(Err(last_error.take().unwrap()))
        } else {
            std::task::Poll::Pending
        }
    })
    .await
}

async fn refresh_descriptors(wanted: SocketAddr) -> anyhow::Result<()> {
    let seeds =
        crate::RESOLVER_SEEDS.get().ok_or_else(|| anyhow::anyhow!("no trusted resolver seeds"))?;
    let relays = race_seed_lookups(seeds.len(), &DISCOVERY_CURSOR, |index| {
        query_descriptors(&seeds[index], wanted)
    })
    .await?;
    crate::data::relay::Relay::refresh(&relays)?;
    Ok(())
}

async fn query_descriptors(
    seed: &crate::data::ResolverSeed, wanted: SocketAddr,
) -> anyhow::Result<Vec<RelayDescriptor>> {
    let address = seed.addr.resolve(DEFAULT_RESOLVER_PORT).await?;
    let connection = crate::quic::dialer::connect(address, &seed.key.to_string()).await?;
    struct Guard(quinn::Connection);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.close(0u32.into(), b"relay lookup complete");
        }
    }
    let connection = Guard(connection);
    let (mut send, mut recv) = connection.0.open_bi().await?;
    send.write_all(&ClientRequest::GetRelays().pack()?).await?;
    send.finish()?;
    let ClientResponse::GetRelays { relays } = ClientResponse::unpack(&mut recv).await? else {
        anyhow::bail!("unexpected relay discovery response");
    };
    anyhow::ensure!(
        relays.iter().any(|relay| {
            super::reflexive::canonical(relay.addr) == super::reflexive::canonical(wanted)
        }),
        "resolver did not describe the offered relay"
    );
    Ok(relays)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LookupLease<'a> {
        active: &'a AtomicUsize,
        dropped: &'a AtomicUsize,
    }

    impl Drop for LookupLease<'_> {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_first_seed_does_not_hide_healthy_later_seed_and_losers_are_dropped() {
        let cursor = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let dropped = AtomicUsize::new(0);
        let (active, dropped) = (&active, &dropped);
        let start = Instant::now();
        let winner = tokio::time::timeout(
            DISCOVERY_DEADLINE,
            race_seed_lookups(3, &cursor, |index| async move {
                active.fetch_add(1, Ordering::Relaxed);
                let _lease = LookupLease { active, dropped };
                match index {
                    0 => std::future::pending().await,
                    1 => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        anyhow::bail!("trusted resolver has no matching descriptor");
                    },
                    _ => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(index)
                    },
                }
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(winner, 2);
        assert_eq!(start.elapsed(), Duration::from_millis(50));
        assert_eq!(active.load(Ordering::Relaxed), 0, "winning lookup must cancel stalled work");
        assert_eq!(dropped.load(Ordering::Relaxed), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn seed_batches_are_bounded_fair_and_cancel_with_the_refresh_budget() {
        let cursor = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let dropped = AtomicUsize::new(0);
        let started = Mutex::new(Vec::new());
        let (active, peak, dropped, started) = (&active, &peak, &dropped, &started);
        for round in 0..3 {
            let result = tokio::time::timeout(
                DISCOVERY_DEADLINE,
                race_seed_lookups(9, &cursor, |index| async move {
                    started.lock().push(index);
                    let count = active.fetch_add(1, Ordering::Relaxed) + 1;
                    peak.fetch_max(count, Ordering::Relaxed);
                    let _lease = LookupLease { active, dropped };
                    std::future::pending::<anyhow::Result<()>>().await
                }),
            )
            .await;
            assert!(result.is_err());
            assert_eq!(active.load(Ordering::Relaxed), 0, "deadline must drop every pending query");
            assert_eq!(started.lock().len(), (round + 1) * DISCOVERY_CONCURRENCY);
        }
        assert_eq!(peak.load(Ordering::Relaxed), DISCOVERY_CONCURRENCY);
        assert_eq!(dropped.load(Ordering::Relaxed), 3 * DISCOVERY_CONCURRENCY);
        assert_eq!(
            started.lock().iter().copied().collect::<std::collections::HashSet<_>>().len(),
            9,
            "later configured seeds must not starve behind an unavailable first batch"
        );
    }

    #[test]
    fn offered_address_never_supplies_its_own_tls_identity() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE relays (id TEXT, host TEXT, port INTEGER);
            INSERT INTO relays VALUES ('trusted-node', '203.0.113.10', 443);
            INSERT INTO relays VALUES ('different-port', '203.0.113.10', 444);",
        )
        .unwrap();
        assert_eq!(
            known_relay_name_in(&db, "203.0.113.10:443".parse().unwrap()).as_deref(),
            Some("trusted-node")
        );
        assert_eq!(
            known_relay_name_in(&db, "[::ffff:203.0.113.10]:443".parse().unwrap()).as_deref(),
            Some("trusted-node")
        );
        assert_eq!(known_relay_name_in(&db, "203.0.113.11:443".parse().unwrap()), None);
        assert_eq!(known_relay_name_in(&db, "203.0.113.10:445".parse().unwrap()), None);
    }

    #[tokio::test(start_paused = true)]
    async fn different_home_relays_share_one_trusted_refresh() {
        let state = tokio::sync::Mutex::new(None);
        let names = Mutex::new(std::collections::HashMap::new());
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let refresh = || async {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Stand in for persistence after authenticated GetRelays; the
            // offered address alone never supplies either trusted name.
            names.lock().insert(1, "first-relay".to_owned());
            names.lock().insert(2, "other-home-relay".to_owned());
            Ok(())
        };
        let (first, other) = tokio::join!(
            refreshed_name(&state, || names.lock().get(&1).cloned(), refresh),
            refreshed_name(&state, || names.lock().get(&2).cloned(), refresh),
        );
        assert_eq!(first.as_deref(), Some("first-relay"));
        assert_eq!(other.as_deref(), Some("other-home-relay"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_relay_lookup_is_bounded_and_cannot_flood_resolvers() {
        let state = tokio::sync::Mutex::new(None);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let start = Instant::now();
        let result = refreshed_name(
            &state,
            || None,
            || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::future::pending::<anyhow::Result<()>>().await
            },
        )
        .await;
        assert_eq!(result, None);
        assert_eq!(start.elapsed(), DISCOVERY_DEADLINE);
        let result = refreshed_name(
            &state,
            || None,
            || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, None);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        tokio::time::advance(DISCOVERY_COOLDOWN).await;
        let result = refreshed_name(
            &state,
            || None,
            || async {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, None, "a successful refresh still cannot invent an unknown identity");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}

fn needed(routes: &Weak<Mutex<TurnRoutes>>, synth: SocketAddr) -> bool {
    routes.upgrade().is_some_and(|routes| routes.lock().needs_tcp(synth))
}

pub(super) fn start(
    routes: &Arc<Mutex<TurnRoutes>>, synth: SocketAddr, token: [u8; 16], relay: SocketAddr,
    peer: [u8; 32],
) {
    if !routes.lock().begin_tcp(&token, synth, relay) {
        return;
    }
    let Some(identity) = Identity::get() else { return };
    let ipk = identity.ipk();
    let weak = Arc::downgrade(routes);
    let worker = crate::RUNTIME.spawn(async move {
        tokio::time::sleep(HEAD_START).await;
        if !needed(&weak, synth) { return; }
        let setup = async {
            let roots = crate::quic::dialer::roots()?;
            let name = refreshed_name(&DISCOVERY, || known_relay_name(relay), || refresh_descriptors(relay))
                .await.ok_or_else(|| anyhow::anyhow!("offered relay has no trusted identity"))?;
            anyhow::ensure!(needed(&weak, synth), "route no longer needs TCP");
            let request = Request::Assist {
                token, ipk, peer,
                sign: Arc::new(move |message| {
                    let (signature, current) = IdentitySigner::sign_with_ipk(message)?;
                    anyhow::ensure!(current == ipk, "identity changed during relay setup");
                    Ok(signature.to_bytes())
                }),
            };
            tunnel::connect(relay, &name, roots, request).await
        };
        let channel = match tokio::time::timeout(JOIN_DEADLINE, setup).await {
            Ok(Ok(channel)) => channel,
            error => {
                diagnostics::record(Event::TcpRelayUnavailable);
                log::debug!("P2P: TCP relay setup unavailable: {error:?}");
                return;
            },
        };
        let Some(routes) = weak.upgrade() else { channel.close(); return };
        if !routes.lock().install_tcp(synth, channel.clone()) { return; }
        drop(routes);
        // Do not duplicate through an unjoined TCP bridge indefinitely. Once
        // authenticated ingress has proved a peer joined, the route owns the
        // channel until its final lease ends; this setup task can finish.
        let deadline = tokio::time::Instant::now() + JOIN_DEADLINE;
        loop {
            let Some(routes) = weak.upgrade() else { channel.close(); return };
            let active = routes.lock().tcp_active(synth);
            if active { return; }
            if !routes.lock().needs_tcp(synth) || tokio::time::Instant::now() >= deadline {
                routes.lock().remove_tcp(synth, &channel);
                return;
            }
            drop(routes);
            tokio::select! {
                _ = channel.closed() => {
                    if let Some(routes) = weak.upgrade() { routes.lock().remove_tcp(synth, &channel); }
                    return;
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {},
            }
        }
    });
    routes.lock().own_tcp_worker(synth, worker);
}
