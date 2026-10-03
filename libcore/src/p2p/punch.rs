//! The hole-punch: disco Ping/Pong with one peer until an address returns a Pong for our Ping,
//! which proves a path in both directions for quinn.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc::Receiver;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::sleep_until;

use super::disco::DiscoKey;
use super::disco::DiscoMsg;
use super::socket::Poke;
use super::socket::PokeSender;

/// Retries within a round, so a lost first packet does not wait out `ROUND_INTERVAL`.
const PROBE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_secs(1),
];
const ROUND_INTERVAL: Duration = Duration::from_secs(5);

/// Caps the addresses one punch pings, offered or heard from, since the peer chooses them.
pub(super) const MAX_CANDIDATES: usize = 16;
/// Leaves room for heard sources that differ from the offer, as behind an endpoint-dependent NAT.
const MAX_ADVERTISED_CANDIDATES: usize = MAX_CANDIDATES - 4;
const MAX_PENDING_PROBES: usize = MAX_CANDIDATES * (PROBE_RETRY_DELAYS.len() + 1);

/// Ignores the IPv4-mapped form a dual-stack socket can report; routing keeps the address as
/// received.
fn same_endpoint(a: SocketAddr, b: SocketAddr) -> bool {
    a.ip().to_canonical() == b.ip().to_canonical() && a.port() == b.port()
}

struct ProbeSchedule {
    next: Instant,
    round_started: Instant,
    attempt: usize,
}

impl ProbeSchedule {
    fn new(now: Instant) -> Self {
        Self { next: now, round_started: now, attempt: 0 }
    }

    /// Advances from the actual send time, so a phone waking late sends one sweep, not a burst.
    fn tick(&mut self, now: Instant) -> bool {
        let new_round = self.attempt == 0;
        if new_round {
            self.round_started = now;
        }
        if let Some(delay) = PROBE_RETRY_DELAYS.get(self.attempt) {
            self.next = now + *delay;
            self.attempt += 1;
        } else {
            self.next = (self.round_started + ROUND_INTERVAL).max(now + PROBE_RETRY_DELAYS[0]);
            self.attempt = 0;
        }
        new_round
    }
}

/// Reserved, local and non-unicast space would only aim our pokes at something other than the
/// peer. Private IPv4 stays: phones on one wifi meet there, and a sealed poke harms no one.
pub(super) fn is_punchable(addr: &SocketAddr) -> bool {
    if addr.port() < 1024 {
        return false;
    }
    match addr.ip().to_canonical() {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_multicast()
                && !v4.is_broadcast()
                && !v4.is_unspecified()
                && !v4.is_documentation()
        },
        IpAddr::V6(v6) => {
            let hi = v6.segments()[0] & 0xffc0;
            !v6.is_loopback()
                && !v6.is_multicast()
                && !v6.is_unspecified()
                && hi != 0xfe80
                && hi != 0xfec0
                && (v6.segments()[0] & 0xfe00) != 0xfc00
        },
    }
}

/// The punch rules for one peer, without I/O: methods return the pokes to send.
struct PunchState {
    key: DiscoKey,
    /// Offered candidates plus any source an inbound Ping came from.
    candidates: Vec<SocketAddr>,
    /// Outstanding Ping challenges and their targets, bounded and cleared each round.
    sent: VecDeque<([u8; 8], SocketAddr)>,
    /// The first address whose Pong matched our Ping.
    validated: Option<SocketAddr>,
    /// Sources of authenticated Pings since the last drain; the peer reaches us from each.
    heard: Vec<SocketAddr>,
}

impl PunchState {
    fn new(key: DiscoKey, candidates: Vec<SocketAddr>) -> Self {
        let mut unique = Vec::with_capacity(MAX_CANDIDATES);
        for addr in candidates {
            if !unique.iter().any(|&known| same_endpoint(known, addr)) {
                unique.push(addr);
                if unique.len() == MAX_ADVERTISED_CANDIDATES {
                    break;
                }
            }
        }
        Self { key, candidates: unique, sent: VecDeque::new(), validated: None, heard: Vec::new() }
    }

    fn start_round(&mut self) {
        self.sent.clear();
    }

    fn tick(&mut self) -> Vec<Poke> {
        self.candidates.clone().into_iter().map(|addr| (addr, self.ping(addr))).collect()
    }

    fn on_poke(&mut self, src: SocketAddr, bytes: &[u8]) -> Vec<Poke> {
        match self.key.open(bytes) {
            Some(DiscoMsg::Ping { tx }) => {
                let learned = !self.candidates.iter().any(|&addr| same_endpoint(addr, src));
                if learned {
                    // The route callback shares this cap, or one authenticated session could grow
                    // the socket's routing table without limit.
                    if self.candidates.len() == MAX_CANDIDATES {
                        return Vec::new();
                    }
                    self.candidates.push(src);
                }
                // The disco key rode only the peer's offer, so this proves the peer reaches us
                // from `src`.
                if !self.heard.iter().any(|&addr| same_endpoint(addr, src)) {
                    self.heard.push(src);
                }
                let mut out = vec![(src, self.key.seal(&DiscoMsg::Pong { tx, seen: src }))];
                // Ping back only on first contact; the tick re-pings after that, so lost Pongs
                // cause no ping-back storm.
                if self.validated.is_none() && learned {
                    out.push((src, self.ping(src)));
                }
                out
            },
            Some(DiscoMsg::Pong { tx, .. }) => {
                if let Some(index) = self
                    .sent
                    .iter()
                    .position(|&(pending, addr)| pending == tx && same_endpoint(addr, src))
                {
                    self.sent.remove(index);
                    self.validated.get_or_insert(src);
                }
                Vec::new()
            },
            None => Vec::new(),
        }
    }

    fn ping(&mut self, addr: SocketAddr) -> Vec<u8> {
        let mut tx = [0u8; 8];
        {
            use ed25519_dalek::ed25519::signature::rand_core::OsRng;
            use ed25519_dalek::ed25519::signature::rand_core::RngCore;
            OsRng.fill_bytes(&mut tx);
        }
        if self.sent.len() == MAX_PENDING_PROBES {
            self.sent.pop_front();
        }
        self.sent.push_back((tx, addr));
        self.key.seal(&DiscoMsg::Ping { tx })
    }
}

/// Returns the first validated address, or `None` after `timeout`. `heard` gets every source an
/// authenticated Ping arrives from, so a bridged session accepts the peer's datagrams there.
pub async fn punch(
    pokes: &PokeSender, inbox: &mut Receiver<Poke>, key: DiscoKey,
    candidates: Vec<SocketAddr>, timeout: Duration, mut heard: impl FnMut(SocketAddr),
) -> Option<SocketAddr> {
    let mut state = PunchState::new(key, candidates);
    let mut schedule = ProbeSchedule::new(Instant::now());
    let deadline = sleep(timeout);
    tokio::pin!(deadline);

    loop {
        let out = tokio::select! {
            biased;
            // The deadline wins over a due sweep, and an inbound flood cannot postpone it.
            _ = &mut deadline => return state.validated,
            _ = sleep_until(schedule.next) => {
                if schedule.tick(Instant::now()) {
                    state.start_round();
                }
                state.tick()
            },
            got = inbox.recv() => match got {
                Some((src, bytes)) => state.on_poke(src, &bytes),
                None => return state.validated, // socket gone
            },
        };
        for src in state.heard.drain(..) {
            heard(src);
        }
        for (addr, bytes) in out {
            tokio::select! {
                biased;
                _ = &mut deadline => return state.validated,
                _ = pokes.send(addr, &bytes) => {},
            }
        }
        if state.validated.is_some() {
            return state.validated;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> DiscoKey {
        DiscoKey::new(&[5; 32], [6; 8])
    }

    fn ping_tx(bytes: &[u8]) -> [u8; 8] {
        match key().open(bytes) {
            Some(DiscoMsg::Ping { tx }) => tx,
            other => panic!("expected a Ping, got {other:?}"),
        }
    }

    fn pong(key: &DiscoKey, tx: [u8; 8]) -> Vec<u8> {
        key.seal(&DiscoMsg::Pong { tx, seen: "9.9.9.9:9".parse().unwrap() })
    }

    #[test]
    fn is_punchable_rejects_local_and_non_unicast() {
        for bad in [
            "127.0.0.1:5000",
            "169.254.1.1:5000",
            "224.0.0.1:5000",
            "255.255.255.255:5000",
            "0.0.0.0:5000",
            "9.9.9.9:53",
            "[::1]:5000",
            "[fe80::1]:5000",
            "[fc00::1]:5000",
            "[ff02::1]:5000",
            "[::ffff:127.0.0.1]:5000",
        ] {
            assert!(!is_punchable(&bad.parse().unwrap()), "{bad} must be rejected");
        }
        for good in [
            "9.9.9.9:5000",
            "[2409:4117::1]:5000",
            "192.168.1.5:5000",
            "10.0.0.1:5000",
            "172.16.0.1:5000",
        ] {
            assert!(is_punchable(&good.parse().unwrap()), "{good} must be allowed");
        }
    }

    #[test]
    fn only_a_pong_from_the_probed_endpoint_for_a_live_challenge_validates() {
        let peer: SocketAddr = "9.9.9.9:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer]);
        let old = ping_tx(&st.tick()[0].1);
        st.start_round();
        let tx = ping_tx(&st.tick()[0].1);
        let other_session = DiscoKey::new(&[4; 32], [6; 8]);
        let other_channel = DiscoKey::new(&[5; 32], [7; 8]);
        for (src, bytes, why) in [
            ("8.8.8.8:5001", pong(&key(), tx), "another host"),
            ("9.9.9.9:5002", pong(&key(), tx), "another port"),
            ("9.9.9.9:5001", pong(&key(), old), "an earlier round"),
            ("9.9.9.9:5001", pong(&key(), [0; 8]), "a challenge never sent"),
            ("9.9.9.9:5001", pong(&other_session, tx), "another session's key"),
            ("9.9.9.9:5001", pong(&other_channel, tx), "another session's channel"),
            ("9.9.9.9:5001", other_session.seal(&DiscoMsg::Ping { tx }), "another session's ping"),
        ] {
            assert!(st.on_poke(src.parse().unwrap(), &bytes).is_empty(), "{why}");
            assert_eq!(st.validated, None, "{why}");
        }
        assert!(st.heard.is_empty(), "no route was learned");
        assert_eq!(st.sent.len(), 1, "a wrong answer does not use up the live challenge");
        let mapped: SocketAddr = "[::ffff:9.9.9.9]:5001".parse().unwrap();
        st.on_poke(mapped, &pong(&key(), tx));
        assert_eq!(st.validated, Some(mapped), "a dual-stack socket reports the peer v4-mapped");
    }

    #[test]
    fn an_inbound_ping_gets_a_pong_and_at_most_one_ping_back() {
        let mut st = PunchState::new(key(), vec![]);
        let src: SocketAddr = "9.9.9.9:6000".parse().unwrap();
        let ping = key().seal(&DiscoMsg::Ping { tx: [7; 8] });
        let out = st.on_poke(src, &ping);
        assert_eq!(out.len(), 2);
        assert!(matches!(key().open(&out[0].1), Some(DiscoMsg::Pong { tx, .. }) if tx == [7; 8]));
        let back = ping_tx(&out[1].1);
        assert_eq!(st.heard, vec![src]);
        assert_eq!(st.on_poke(src, &ping).len(), 1, "a repeated ping gets no second ping-back");
        st.on_poke(src, &pong(&key(), back));
        assert_eq!(st.validated, Some(src));
        let other = key().seal(&DiscoMsg::Ping { tx: [1; 8] });
        assert_eq!(
            st.on_poke("9.9.9.9:7000".parse().unwrap(), &other).len(),
            1,
            "validated: Pong only"
        );
    }
}
