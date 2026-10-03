//! Correlates the relay's address echoes with outstanding requests. This is return-path
//! correlation, not cryptographic authentication of a relay.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use super::socket::StunReply;

const MAX_PENDING: usize = 16;

struct Pending {
    relay: SocketAddr,
    deadline: Instant,
    reply: oneshot::Sender<SocketAddr>,
}

#[derive(Default)]
pub(super) struct Reflexive {
    cached: Option<(SocketAddr, SocketAddr, Instant)>,
    pending: HashMap<[u8; 8], Pending>,
}

pub(super) fn canonical(addr: SocketAddr) -> SocketAddr {
    // Preserve a native IPv6 scope id; only normalize IPv4-mapped addresses.
    if addr.is_ipv6() && addr.ip().to_canonical().is_ipv4() {
        SocketAddr::new(addr.ip().to_canonical(), addr.port())
    } else {
        addr
    }
}

impl Reflexive {
    pub fn cached(&self, relay: SocketAddr, max_age: Duration) -> Option<SocketAddr> {
        self.cached.filter(|(source, _, at)| *source == canonical(relay) && at.elapsed() < max_age)
            .map(|(_, seen, _)| seen)
    }

    pub fn begin(
        &mut self, tx: [u8; 8], relay: SocketAddr, lifetime: Duration,
    ) -> Option<oneshot::Receiver<SocketAddr>> {
        let now = Instant::now();
        self.pending.retain(|_, p| p.deadline > now && !p.reply.is_closed());
        if self.pending.len() >= MAX_PENDING || self.pending.contains_key(&tx) {
            return None;
        }
        let (reply, rx) = oneshot::channel();
        self.pending.insert(tx, Pending { relay: canonical(relay), deadline: now + lifetime, reply });
        Some(rx)
    }

    pub fn cancel(&mut self, tx: &[u8; 8]) {
        self.pending.remove(tx);
    }

    pub fn accept(&mut self, (source, tx, seen): StunReply) {
        let seen = canonical(seen);
        let Some(pending) = self.pending.get(&tx) else { return };
        if pending.deadline <= Instant::now() || pending.reply.is_closed() {
            self.pending.remove(&tx);
            return;
        }
        // A mismatched source must not consume the genuine outstanding query.
        if canonical(source) != pending.relay || seen.port() == 0
            || seen.ip().is_unspecified() || seen.ip().is_multicast()
        {
            return;
        }
        let pending = self.pending.remove(&tx).expect("pending query");
        self.cached = Some((pending.relay, seen, Instant::now()));
        let _ = pending.reply.send(seen);
    }

    /// Clears pending queries too, so a late reply from the old network cannot refill the cache.
    pub fn invalidate(&mut self) {
        self.cached = None;
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_echo_counts_only_from_the_exact_relay_for_a_live_query() {
        let relay: SocketAddr = "8.8.8.8:443".parse().unwrap();
        let seen: SocketAddr = "9.9.9.9:54321".parse().unwrap();
        let age = Duration::from_secs(60);
        let mut state = Reflexive::default();
        let mut live = state.begin([1; 8], relay, Duration::from_secs(1)).unwrap();
        for (source, tx, echoed) in [
            (relay, [2; 8], seen),
            ("8.8.4.4:443".parse().unwrap(), [1; 8], seen),
            ("8.8.8.8:444".parse().unwrap(), [1; 8], seen),
            (relay, [1; 8], "[::ffff:0.0.0.0]:54321".parse().unwrap()),
            (relay, [1; 8], "[::ffff:224.0.0.1]:54321".parse().unwrap()),
        ] {
            state.accept((source, tx, echoed));
            assert!(live.try_recv().is_err(), "{source} {tx:?} {echoed}");
        }
        assert_eq!(state.cached(relay, age), None);
        state.accept(("[::ffff:8.8.8.8]:443".parse().unwrap(), [1; 8], seen));
        assert_eq!(live.try_recv().unwrap(), seen);
        assert_eq!(state.cached(relay, age), Some(seen));
        state.accept((relay, [1; 8], "9.9.9.9:12345".parse().unwrap()));
        assert_eq!(state.cached(relay, age), Some(seen), "the first answer sticks");

        let mut state = Reflexive::default();
        let mut expired = state.begin([3; 8], relay, Duration::ZERO).unwrap();
        state.accept((relay, [3; 8], seen));
        let mut cancelled = state.begin([4; 8], relay, Duration::from_secs(1)).unwrap();
        state.cancel(&[4; 8]);
        state.accept((relay, [4; 8], seen));
        let mut previous = state.begin([5; 8], relay, Duration::from_secs(1)).unwrap();
        state.invalidate();
        state.accept((relay, [5; 8], seen));
        for rx in [&mut expired, &mut cancelled, &mut previous] {
            assert!(rx.try_recv().is_err());
        }
        assert_eq!(state.cached(relay, age), None, "no stale reply refills the cache");
    }
}
