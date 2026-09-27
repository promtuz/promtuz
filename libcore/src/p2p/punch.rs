//! The hole-punch: drive disco Ping/Pong to open a NAT hole to one peer
//! and report the first address that answers — a validated, bidirectional
//! path we can hand to quinn.
//!
//! The rule set is small (see the spec / the design notes): ping every
//! candidate (opens our NAT toward it); on an inbound Ping, Pong the
//! source, and the *first* time we hear from a peer we haven't validated,
//! Ping it back so both directions get proven even if one ping is lost;
//! on a Pong that matches a Ping we sent, that address is validated.
//!
//! [`PunchState`] is the pure rule set — `tick`/`on_poke` return the pokes
//! to send, no I/O — and [`punch`] is the async shell that sends them and
//! feeds inbound ones from the socket.

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

/// Retry promptly within a round, so a lost initial packet does not cost five
/// seconds. Six probes per candidate take 2.5s; repeated rounds have a separate
/// five-second minimum spacing. These are bounded starting values, not a claim
/// about the best timing for every carrier/NAT.
const PROBE_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_secs(1),
];
const ROUND_INTERVAL: Duration = Duration::from_secs(5);

/// Ceiling on the addresses one punch will ever ping — the peer's offer plus
/// the sources it is heard from. Every candidate is a datagram per tick toward
/// an address the peer chose, so the fanout is capped rather than trusted.
pub(super) const MAX_CANDIDATES: usize = 16;
/// Keep room for authenticated sources that differ from advertised addresses
/// (e.g. an endpoint-dependent NAT). A full offer must not exclude those paths.
const MAX_ADVERTISED_CANDIDATES: usize = MAX_CANDIDATES - 4;
const MAX_PENDING_PROBES: usize = MAX_CANDIDATES * (PROBE_RETRY_DELAYS.len() + 1);

/// Ignore IPv4's mapped-v6 representation when matching a return path. Keep
/// the actual received address for routing: a dual-stack socket can report a
/// mapped address even when the advertised candidate was ordinary IPv4.
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

    /// Advance from the actual send time. A delayed task sends one sweep,
    /// never a burst of overdue interval ticks after the phone wakes up.
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

/// Whether a peer-supplied candidate is an address we are willing to send to.
/// Reserved, local and non-unicast space is never a peer's reachable address,
/// only a way to aim our pokes at something that isn't the peer. Private
/// IPv4 stays: two phones on one wifi reach each other through it, and a
/// sealed poke at a LAN address the peer named is harmless to anyone else.
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

/// The punch rule set for one peer. No I/O: every method returns the
/// pokes the shell should send.
struct PunchState {
    key: DiscoKey,
    /// Addresses to ping — the peer's advertised candidates, plus any
    /// source we hear an inbound Ping from.
    candidates: Vec<SocketAddr>,
    /// Challenge and target address, bounded even if the state is driven
    /// beyond the normal round budget. Previous-round challenges expire.
    sent: VecDeque<([u8; 8], SocketAddr)>,
    /// First address to answer a Pong. Once set we stop pinging back.
    validated: Option<SocketAddr>,
    /// Sources we have taken an authenticated Ping from since the last
    /// drain. The peer can reach us from each, so each is an address they
    /// may flip their own egress to even if we never validate one.
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

    /// Ping every candidate — one round, opens/refreshes our NAT toward
    /// each.
    fn tick(&mut self) -> Vec<Poke> {
        self.candidates.clone().into_iter().map(|addr| (addr, self.ping(addr))).collect()
    }

    /// Handle one inbound poke.
    fn on_poke(&mut self, src: SocketAddr, bytes: &[u8]) -> Vec<Poke> {
        match self.key.open(bytes) {
            Some(DiscoMsg::Ping { tx }) => {
                let learned = !self.candidates.iter().any(|&addr| same_endpoint(addr, src));
                if learned {
                    // The route callback must share the candidate cap too:
                    // otherwise one authenticated session can grow the
                    // socket's real-address routing table without limit.
                    if self.candidates.len() == MAX_CANDIDATES {
                        return Vec::new();
                    }
                    self.candidates.push(src);
                }
                // Sealed with the session's disco key, which only rode the
                // peer's offer — hearing this is proof the peer reaches us
                // from `src`, whatever our own pings toward them do.
                if !self.heard.iter().any(|&addr| same_endpoint(addr, src)) {
                    self.heard.push(src);
                }
                let mut out = vec![(src, self.key.seal(&DiscoMsg::Pong { tx, seen: src }))];
                // Ping back only the first time we hear from a not-yet-
                // validated peer; after that the tick re-pings it. Gating
                // on `learned` stops a ping-back storm if Pongs are lost.
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
            // Not our channel, or failed authentication — ignore.
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

/// Punch a hole to `candidates`, returning the first validated address or
/// `None` after `timeout`. Sends pokes via `pokes`; consumes inbound
/// pokes (for this session) from `inbox`.
///
/// Returns as soon as one address validates — that path is bidirectionally
/// open, and the caller (dialer) connects to it while QUIC's own packets
/// keep the hole alive. The accepting side runs this too, purely to open
/// its own NAT, and accepts the incoming connection regardless.
///
/// `heard` is called with admitted sources an authenticated Ping arrives from,
/// validated or not: the peer reaches us from there, so a bridged session
/// must accept their datagrams at that address even when our own punch
/// never validates one (see `TurnRoutes::accept_from`).
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
            // Never send a boundary sweep when timeout and retry are both
            // ready; an inbound flood must not postpone expiry either.
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
        DiscoKey::new(&[5u8; 32], [6u8; 8])
    }
    fn open_ping(bytes: &[u8]) -> [u8; 8] {
        match key().open(bytes) {
            Some(DiscoMsg::Ping { tx }) => tx,
            other => panic!("expected Ping, got {other:?}"),
        }
    }

    #[test]
    fn tick_pings_every_candidate() {
        let a: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:5002".parse().unwrap();
        let mut st = PunchState::new(key(), vec![a, b]);
        let pokes = st.tick();
        assert_eq!(pokes.iter().map(|p| p.0).collect::<Vec<_>>(), vec![a, b]);
        // both are real Pings, and both tx_ids are recorded as sent
        for (_, bytes) in &pokes {
            open_ping(bytes);
        }
        assert_eq!(st.sent.len(), 2);
    }

    #[test]
    fn matching_pong_validates() {
        let peer: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer]);
        let tx = open_ping(&st.tick()[0].1);

        let pong = key().seal(&DiscoMsg::Pong { tx, seen: "127.0.0.1:9".parse().unwrap() });
        let out = st.on_poke(peer, &pong);
        assert!(out.is_empty());
        assert_eq!(st.validated, Some(peer));

        // an unknown tx does not validate
        let mut st2 = PunchState::new(key(), vec![peer]);
        let stray = key().seal(&DiscoMsg::Pong { tx: [0; 8], seen: peer });
        st2.on_poke(peer, &stray);
        assert_eq!(st2.validated, None);
    }

    #[test]
    fn pong_must_return_from_the_probed_endpoint() {
        let peer: SocketAddr = "9.9.9.9:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer]);
        let tx = open_ping(&st.tick()[0].1);
        let pong = key().seal(&DiscoMsg::Pong { tx, seen: peer });
        for other in ["8.8.8.8:5001", "9.9.9.9:5002"] {
            st.on_poke(other.parse().unwrap(), &pong);
            assert_eq!(st.validated, None);
            assert_eq!(st.sent.len(), 1, "wrong source must not consume a valid challenge");
        }

        // Dual-stack sockets can report the same IPv4 peer as mapped IPv6.
        let received: SocketAddr = "[::ffff:9.9.9.9]:5001".parse().unwrap();
        st.on_poke(received, &pong);
        assert_eq!(st.validated, Some(received));
        assert!(st.sent.is_empty());
    }

    #[test]
    fn old_round_and_other_session_cannot_validate_or_add_routes() {
        let peer: SocketAddr = "9.9.9.9:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer]);
        let old_tx = open_ping(&st.tick()[0].1);
        st.start_round();
        let tx = open_ping(&st.tick()[0].1);

        st.on_poke(peer, &key().seal(&DiscoMsg::Pong { tx: old_tx, seen: peer }));
        for other_session in [DiscoKey::new(&[4; 32], [6; 8]), DiscoKey::new(&[5; 32], [7; 8])] {
            st.on_poke(peer, &other_session.seal(&DiscoMsg::Pong { tx, seen: peer }));
            assert!(st.on_poke(peer, &other_session.seal(&DiscoMsg::Ping { tx })).is_empty());
        }
        assert_eq!(st.validated, None);
        assert!(st.heard.is_empty());
        assert_eq!(st.sent.len(), 1);
        st.on_poke(peer, &key().seal(&DiscoMsg::Pong { tx, seen: peer }));
        assert_eq!(st.validated, Some(peer));
    }

    #[test]
    fn retry_round_recovers_lost_packets_without_waiting_five_seconds() {
        let peer: SocketAddr = "9.9.9.9:5001".parse().unwrap();
        let start = Instant::now();
        let mut schedule = ProbeSchedule::new(start);
        let mut st = PunchState::new(key(), vec![peer]);

        // Both sides may lose an initial Ping or its Pong. Keep earlier
        // challenges alive within the round, so a delayed reply still works.
        let mut delayed = None;
        for elapsed in [0, 100, 300] {
            let now = start + Duration::from_millis(elapsed);
            assert_eq!(schedule.next, now);
            if schedule.tick(now) {
                st.start_round();
            }
            let tx = open_ping(&st.tick()[0].1);
            if elapsed == 100 {
                delayed = Some(tx);
            }
        }
        st.on_poke(peer, &key().seal(&DiscoMsg::Pong { tx: delayed.unwrap(), seen: peer }));
        assert_eq!(st.validated, Some(peer));
    }

    #[test]
    fn retry_budget_and_round_spacing_survive_delayed_polling() {
        let start = Instant::now();
        let mut schedule = ProbeSchedule::new(start);
        for (index, elapsed) in [0, 100, 300, 700, 1500, 2500].into_iter().enumerate() {
            let now = start + Duration::from_millis(elapsed);
            assert_eq!(schedule.next, now);
            assert_eq!(schedule.tick(now), index == 0);
        }
        assert_eq!(schedule.next, start + ROUND_INTERVAL);
        assert!(schedule.tick(start + ROUND_INTERVAL));

        // Waking much later does not queue all missed probes or rounds.
        let later = start + Duration::from_secs(30);
        assert!(!schedule.tick(later));
        assert!(schedule.next > later);
        for _ in 0..10 {
            let now = schedule.next + Duration::from_secs(10);
            schedule.tick(now);
            assert!(schedule.next > now);
        }
    }

    #[test]
    fn inbound_ping_pongs_then_pings_back_once() {
        let mut st = PunchState::new(key(), vec![]);
        let src: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let ping = key().seal(&DiscoMsg::Ping { tx: [7; 8] });

        // first contact: Pong (echoing tx) + one ping-back; src is learned
        let out = st.on_poke(src, &ping);
        assert_eq!(out.len(), 2);
        assert!(matches!(key().open(&out[0].1), Some(DiscoMsg::Pong { tx, .. }) if tx == [7; 8]));
        open_ping(&out[1].1);
        assert!(st.candidates.contains(&src));

        // second ping from the same src: Pong only, no ping-back storm
        let out = st.on_poke(src, &ping);
        assert_eq!(out.len(), 1);
        assert!(matches!(key().open(&out[0].1), Some(DiscoMsg::Pong { .. })));
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
            // the same loopback, arriving v4-mapped off a dual-stack socket
            "[::ffff:127.0.0.1]:5000",
        ] {
            assert!(!is_punchable(&bad.parse().unwrap()), "{bad} must be rejected");
        }
        for good in [
            "9.9.9.9:5000",
            "[2409:4117::1]:5000",
            // LAN peers: what the gatherer publishes for a shared wifi
            "192.168.1.5:5000",
            "10.0.0.1:5000",
            "172.16.0.1:5000",
        ] {
            assert!(is_punchable(&good.parse().unwrap()), "{good} must be allowed");
        }
    }

    #[test]
    fn candidate_list_is_capped() {
        let many: Vec<SocketAddr> =
            (0..1000u16).map(|i| SocketAddr::from(([9, 9, 9, 9], 5000 + i))).collect();
        let mut st = PunchState::new(key(), many);
        assert_eq!(st.candidates.len(), MAX_ADVERTISED_CANDIDATES);
        assert_eq!(st.tick().len(), MAX_ADVERTISED_CANDIDATES);

        // Even a full advertised list leaves room for actual NAT sources.
        for port in 6000..6004 {
            let src = SocketAddr::from(([8, 8, 8, 8], port));
            assert_eq!(st.on_poke(src, &key().seal(&DiscoMsg::Ping { tx: [2; 8] })).len(), 2);
        }
        assert_eq!(st.heard.len(), 4);
        st.heard.clear();

        let extra: SocketAddr = "8.8.8.8:6004".parse().unwrap();
        assert!(st.on_poke(extra, &key().seal(&DiscoMsg::Ping { tx: [2; 8] })).is_empty());
        assert_eq!(st.candidates.len(), MAX_CANDIDATES);
        assert!(st.heard.is_empty(), "overflow must not populate the socket's route table");

        for _ in 0..100 {
            st.tick();
            assert!(st.sent.len() <= MAX_PENDING_PROBES);
        }
        st.start_round();
        assert!(st.sent.is_empty());
    }

    #[test]
    fn duplicate_candidates_and_pings_do_not_exhaust_address_budget() {
        let peer: SocketAddr = "9.9.9.9:5001".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:9.9.9.9]:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer, peer, mapped]);
        assert_eq!(st.tick().len(), 1);
        let ping = key().seal(&DiscoMsg::Ping { tx: [7; 8] });
        for _ in 0..100 {
            assert_eq!(st.on_poke(mapped, &ping).len(), 1);
        }
        assert_eq!(st.candidates.len(), 1);
        assert_eq!(st.heard, vec![mapped]);
    }

    #[test]
    fn validated_ping_does_not_ping_back() {
        let peer: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let mut st = PunchState::new(key(), vec![peer]);
        let tx = open_ping(&st.tick()[0].1);
        st.on_poke(peer, &key().seal(&DiscoMsg::Pong { tx, seen: peer }));
        assert!(st.validated.is_some());

        // new peer pings after we're validated → Pong only
        let other: SocketAddr = "127.0.0.1:7000".parse().unwrap();
        let out = st.on_poke(other, &key().seal(&DiscoMsg::Ping { tx: [1; 8] }));
        assert_eq!(out.len(), 1);
        assert!(matches!(key().open(&out[0].1), Some(DiscoMsg::Pong { .. })));
    }

    #[tokio::test]
    async fn local_socket_retries_after_a_lost_ping() {
        use super::super::socket::PunchSocket;
        use tokio::net::UdpSocket;
        use tokio::sync::mpsc;
        use tokio::time::timeout;

        let bound = PunchSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            punch(&bound.pokes, &mut rx, key(), vec![peer_addr], Duration::from_secs(2), |_| {})
                .await
        });
        let mut buf = [0; 256];
        // Drop the first packet, then reply to the first retry. This exercises
        // the async shell and actual datagram sending, not just its schedule.
        timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        let (len, _) =
            timeout(Duration::from_secs(1), peer.recv_from(&mut buf)).await.unwrap().unwrap();
        let challenge = open_ping(&buf[..len]);
        tx.send((peer_addr, key().seal(&DiscoMsg::Pong { tx: challenge, seen: peer_addr })))
            .await
            .unwrap();
        assert_eq!(timeout(Duration::from_secs(1), task).await.unwrap().unwrap(), Some(peer_addr));
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_wins_over_retry_and_queued_authenticated_packets() {
        use super::super::socket::PunchSocket;
        use tokio::net::UdpSocket;
        use tokio::sync::mpsc;

        let bound = PunchSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = peer.local_addr().unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        tx.send((addr, key().seal(&DiscoMsg::Ping { tx: [7; 8] }))).await.unwrap();
        let result = punch(&bound.pokes, &mut rx, key(), vec![addr], Duration::ZERO, |_| {
            panic!("expired session must not admit a route")
        })
        .await;
        assert_eq!(result, None);
        assert!(
            matches!(peer.try_recv_from(&mut [0; 256]), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_punch_stops_receiving_session_packets() {
        use super::super::socket::PunchSocket;
        use tokio::sync::mpsc;

        let bound = PunchSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let (tx, mut rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            punch(&bound.pokes, &mut rx, key(), Vec::new(), Duration::from_secs(10), |_| {}).await
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(tx.is_closed());
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(tx.is_closed());
    }
}
