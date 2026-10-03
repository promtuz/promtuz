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
use parking_lot::Mutex;
use tokio::time::Instant;

use super::diagnostics::{self, Event};
use super::socket::TurnRoutes;
use crate::data::identity::{Identity, IdentitySigner};
use crate::state::core;

const HEAD_START: Duration = Duration::from_millis(600);
const JOIN_DEADLINE: Duration = Duration::from_secs(8);
const DISCOVERY_DEADLINE: Duration = Duration::from_secs(3);
const DISCOVERY_COOLDOWN: Duration = Duration::from_secs(60);
const DISCOVERY_CONCURRENCY: usize = 4;

/// A peer may name a relay address, but never supplies the identity trusted by
/// TLS. Only our resolver-authenticated local relay records supply that name.
fn known_relay_name(relay: SocketAddr) -> Option<String> {
    let db = core().db.network().lock();
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

/// Serializes and throttles unknown-relay refreshes, so peer offers cannot drive unbounded
/// resolver traffic.
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

/// Races whole lookups, not just connections, so a resolver that never answers cannot stall
/// another seed. The rotating batch bounds the work and gives every seed a turn.
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
    let seeds = &core()
        .net
        .get()
        .ok_or_else(|| anyhow::anyhow!("no trusted resolver seeds"))?
        .seeds;
    let relays = race_seed_lookups(seeds.len(), &core().p2p.discovery_cursor, |index| {
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
    let worker = core().spawn(async move {
        tokio::time::sleep(HEAD_START).await;
        if !needed(&weak, synth) { return; }
        let setup = async {
            let roots = crate::quic::dialer::roots()?;
            let discovery = &core().p2p.relay_discovery;
            let name = refreshed_name(discovery, || known_relay_name(relay), || refresh_descriptors(relay))
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
        // An unjoined bridge is dropped after JOIN_DEADLINE. Once authenticated ingress proves
        // the peer joined, the route owns the channel and this task ends.
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
    routes.lock().own_tcp_worker(synth, worker.abort_handle());
}

#[cfg(test)]
mod tests {
    use common::proto::client_res::RelayDescriptor;
    use common::types::bytes::Bytes;
    use common::types::id::NodeId;

    use super::*;

    #[test]
    fn offered_address_never_supplies_its_own_tls_identity() {
        let db = crate::test_support::data::open(crate::db::network::migrate);
        let relay = |key: u8, addr: &str| RelayDescriptor {
            id:     NodeId::new([key; 32]),
            addr:   addr.parse().unwrap(),
            pubkey: Bytes([key; 32]),
        };
        let (trusted, other_port) = (relay(1, "203.0.113.10:443"), relay(2, "203.0.113.10:444"));
        crate::data::relay::Relay::refresh_tx(&db, &[trusted.clone(), other_port]).unwrap();
        let name = |addr: &str| known_relay_name_in(&db, addr.parse().unwrap());
        assert_eq!(name("203.0.113.10:443"), Some(trusted.id.to_string()));
        assert_eq!(name("[::ffff:203.0.113.10]:443"), Some(trusted.id.to_string()));
        assert_eq!(name("203.0.113.11:443"), None);
        assert_eq!(name("203.0.113.10:445"), None);
    }
}
