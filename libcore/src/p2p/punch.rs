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

use std::collections::HashMap;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::interval;
use tokio::time::sleep;

use super::disco::DiscoKey;
use super::disco::DiscoMsg;
use super::socket::Poke;
use super::socket::PokeSender;

/// How often we re-ping candidates while still trying (iroh's number).
/// The first tick fires immediately, so punching starts at once.
const PING_INTERVAL: Duration = Duration::from_secs(5);

/// Ceiling on the addresses one punch will ever ping — the peer's offer plus
/// the sources it is heard from. Every candidate is a datagram per tick toward
/// an address the peer chose, so the fanout is capped rather than trusted.
pub(super) const MAX_CANDIDATES: usize = 16;

/// Whether a peer-supplied candidate is an address we are willing to send to.
/// Reserved, local and non-unicast space is never a peer's reachable address,
/// only a way to aim our pokes at something that isn't the peer.
pub(super) fn is_punchable(addr: &SocketAddr) -> bool {
    if addr.port() < 1024 {
        return false;
    }
    match addr.ip() {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_private()
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
    /// tx_id → the address we sent that Ping to, so a matching Pong tells
    /// us which address is reachable.
    sent: HashMap<[u8; 8], SocketAddr>,
    /// First address to answer a Pong. Once set we stop pinging back.
    validated: Option<SocketAddr>,
}

impl PunchState {
    fn new(key: DiscoKey, mut candidates: Vec<SocketAddr>) -> Self {
        candidates.truncate(MAX_CANDIDATES);
        Self { key, candidates, sent: HashMap::new(), validated: None }
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
                let mut out = vec![(src, self.key.seal(&DiscoMsg::Pong { tx, seen: src }))];
                let learned = !self.candidates.contains(&src)
                    && self.candidates.len() < MAX_CANDIDATES;
                if learned {
                    self.candidates.push(src);
                }
                // Ping back only the first time we hear from a not-yet-
                // validated peer; after that the tick re-pings it. Gating
                // on `learned` stops a ping-back storm if Pongs are lost.
                if self.validated.is_none() && learned {
                    out.push((src, self.ping(src)));
                }
                out
            }
            Some(DiscoMsg::Pong { tx, .. }) => {
                if let Some(addr) = self.sent.remove(&tx) {
                    self.validated.get_or_insert(addr);
                }
                Vec::new()
            }
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
        self.sent.insert(tx, addr);
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
pub async fn punch(
    pokes: &PokeSender,
    inbox: &mut UnboundedReceiver<Poke>,
    key: DiscoKey,
    candidates: Vec<SocketAddr>,
    timeout: Duration,
) -> Option<SocketAddr> {
    let mut state = PunchState::new(key, candidates);
    let mut ticker = interval(PING_INTERVAL);
    let deadline = sleep(timeout);
    tokio::pin!(deadline);

    loop {
        let out = tokio::select! {
            _ = ticker.tick() => state.tick(),
            got = inbox.recv() => match got {
                Some((src, bytes)) => state.on_poke(src, &bytes),
                None => return state.validated, // socket gone
            },
            _ = &mut deadline => return state.validated,
        };
        for (addr, bytes) in out {
            let _ = pokes.send(addr, &bytes).await;
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
            "192.168.1.5:5000",
            "10.0.0.1:5000",
            "172.16.0.1:5000",
            "169.254.1.1:5000",
            "224.0.0.1:5000",
            "255.255.255.255:5000",
            "0.0.0.0:5000",
            "9.9.9.9:53",
            "[::1]:5000",
            "[fe80::1]:5000",
            "[fc00::1]:5000",
            "[ff02::1]:5000",
        ] {
            assert!(!is_punchable(&bad.parse().unwrap()), "{bad} must be rejected");
        }
        for good in ["9.9.9.9:5000", "[2409:4117::1]:5000"] {
            assert!(is_punchable(&good.parse().unwrap()), "{good} must be allowed");
        }
    }

    #[test]
    fn candidate_list_is_capped() {
        let many: Vec<SocketAddr> =
            (0..1000u16).map(|i| SocketAddr::from(([9, 9, 9, 9], 5000 + i))).collect();
        let mut st = PunchState::new(key(), many);
        assert_eq!(st.candidates.len(), MAX_CANDIDATES);
        assert_eq!(st.tick().len(), MAX_CANDIDATES);

        let extra: SocketAddr = "8.8.8.8:6000".parse().unwrap();
        st.on_poke(extra, &key().seal(&DiscoMsg::Ping { tx: [2; 8] }));
        assert_eq!(st.candidates.len(), MAX_CANDIDATES);
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
}
