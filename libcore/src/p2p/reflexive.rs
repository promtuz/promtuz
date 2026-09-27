//! Correlate the relay's address echoes with outstanding requests. This is
//! return-path correlation, not cryptographic authentication of a relay.

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

    /// A network change invalidates both cached mappings and outstanding
    /// transactions; late replies from the old network cannot repopulate it.
    pub fn invalidate(&mut self) {
        self.cached = None;
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_require_live_transaction_and_exact_relay_source() {
        let relay = "8.8.8.8:443".parse().unwrap();
        let seen = "9.9.9.9:54321".parse().unwrap();
        let mut state = Reflexive::default();
        let mut rx = state.begin([1; 8], relay, Duration::from_secs(1)).unwrap();
        state.accept((relay, [2; 8], seen));
        state.accept(("8.8.4.4:443".parse().unwrap(), [1; 8], seen));
        state.accept(("8.8.8.8:444".parse().unwrap(), [1; 8], seen));
        state.accept((relay, [1; 8], "[::ffff:0.0.0.0]:54321".parse().unwrap()));
        state.accept((relay, [1; 8], "[::ffff:224.0.0.1]:54321".parse().unwrap()));
        assert!(rx.try_recv().is_err());
        assert!(state.cached(relay, Duration::from_secs(60)).is_none());
        state.accept(("[::ffff:8.8.8.8]:443".parse().unwrap(), [1; 8], seen));
        assert_eq!(rx.try_recv().unwrap(), seen);
        assert_eq!(state.cached(relay, Duration::from_secs(60)), Some(seen));
        state.accept((relay, [1; 8], "9.9.9.9:12345".parse().unwrap()));
        assert_eq!(state.cached(relay, Duration::from_secs(60)), Some(seen));
    }

    #[test]
    fn stale_cancelled_and_previous_network_replies_never_refresh_cache() {
        let relay = "8.8.8.8:443".parse().unwrap();
        let seen = "9.9.9.9:54321".parse().unwrap();
        let mut state = Reflexive::default();
        let mut expired = state.begin([1; 8], relay, Duration::ZERO).unwrap();
        state.accept((relay, [1; 8], seen));
        assert!(expired.try_recv().is_err());
        let mut cancelled = state.begin([2; 8], relay, Duration::from_secs(1)).unwrap();
        state.cancel(&[2; 8]);
        state.accept((relay, [2; 8], seen));
        assert!(cancelled.try_recv().is_err());
        let mut old = state.begin([3; 8], relay, Duration::from_secs(1)).unwrap();
        state.invalidate();
        let mut current = state.begin([4; 8], relay, Duration::from_secs(1)).unwrap();
        state.accept((relay, [3; 8], seen));
        assert!(old.try_recv().is_err());
        assert!(current.try_recv().is_err());
        assert!(state.cached(relay, Duration::from_secs(60)).is_none());
        state.accept((relay, [4; 8], seen));
        assert_eq!(current.try_recv().unwrap(), seen);
    }

    #[test]
    fn concurrent_queries_remain_separate_and_bounded() {
        let relay = "8.8.8.8:443".parse().unwrap();
        let mut state = Reflexive::default();
        let mut receivers = Vec::new();
        for i in 0..MAX_PENDING as u8 {
            receivers.push(state.begin([i; 8], relay, Duration::from_secs(10)).unwrap());
        }
        assert!(state.begin([255; 8], relay, Duration::from_secs(10)).is_none());
        let seen = "9.9.9.9:54321".parse().unwrap();
        state.accept((relay, [0; 8], seen));
        assert_eq!(receivers[0].try_recv().unwrap(), seen);
        assert!(receivers[1].try_recv().is_err());
        assert!(state.begin([255; 8], relay, Duration::from_secs(10)).is_some());
        state.invalidate();
        assert!(state.cached(relay, Duration::from_secs(60)).is_none());
    }
}
