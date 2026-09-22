//! P2P signaling: trade candidate addresses over the existing MLS
//! channel — the peer-to-peer analogue of "here's where to reach me",
//! but E2E and authenticated for free, so it replaces a relay-carried
//! call-me-maybe.
//!
//! A session started by the transport calls [`listen`] for the peer's
//! offer and [`send_offer`] to publish its own; the inbound MLS dispatch
//! (`quic/server.rs`) routes the peer's offer here via [`deliver`], keyed
//! by peer IPK. An offer that arrives before its session is listening is
//! buffered ([`PENDING`]) so a slightly-late `connect` still sees it —
//! the two peers rarely tap at the same instant.
//!
//! Every offer names the connect attempt it belongs to and the moment it
//! stops being worth answering. The relay may hold a dispatch for a peer
//! who is away; without the expiry that peer would come back to a queue of
//! bridges nobody waits on and burn a session on each.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Result;
use common::proto::mls_wire::AppPayload;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::utils::addr_short;
use crate::utils::addrs_short;

/// Tolerated clock difference between the two phones when judging an
/// offer's expiry.
const CLOCK_SKEW_MS: u64 = 5_000;

/// A peer's connection offer: where to reach them directly, their home relay
/// for the TURN fallback, and random session secrets — a bridge token and a
/// disco key (the dialer's win).
#[derive(Debug, Clone)]
pub struct Offer {
    /// The peer's connect attempt this offer belongs to.
    pub session:       [u8; 16],
    /// Our attempt this offer answers, when it answers one.
    pub in_reply_to:   Option<[u8; 16]>,
    /// The peer's clock; past it the bridge and the secrets are dead.
    pub expires_at_ms: u64,
    pub candidates:    Vec<SocketAddr>,
    pub relay:         Option<SocketAddr>,
    pub token:         [u8; 16],
    pub disco_key:     [u8; 32],
}

impl Offer {
    pub fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms <= self.expires_at_ms.saturating_add(CLOCK_SKEW_MS)
    }

    /// Whether a session `mine` that is answering `answering` (if any) can
    /// pair with this offer: it must be fresh, and either unsolicited, a
    /// reply to us, or the very offer we set out to answer.
    pub fn matches(&self, mine: [u8; 16], answering: Option<[u8; 16]>, now_ms: u64) -> bool {
        self.is_fresh(now_ms)
            && (self.in_reply_to.is_none()
                || self.in_reply_to == Some(mine)
                || answering == Some(self.session))
    }
}

/// Peer IPK → the live session waiting for that peer's candidate offers.
type Listeners = Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<Offer>>>;
static LISTENERS: Lazy<Listeners> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Offers that arrived before their session was listening. The next
/// [`listen`] drains an entry if it is still fresh and drops it otherwise;
/// the expiry is the freshness bound.
static PENDING: Lazy<Mutex<HashMap<[u8; 32], Offer>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn now_ms() -> u64 {
    crate::utils::systime().as_millis() as u64
}

/// Start listening for `peer`'s candidate offers. Returns the receiver and
/// the sender that identifies this listener to [`stop`]; any fresh offer
/// that already arrived is delivered immediately.
pub fn listen(peer: [u8; 32]) -> (mpsc::UnboundedReceiver<Offer>, mpsc::UnboundedSender<Offer>) {
    let (tx, rx) = mpsc::unbounded_channel();
    LISTENERS.lock().insert(peer, tx.clone());
    if let Some(buffered) = PENDING.lock().remove(&peer) {
        if buffered.is_fresh(now_ms()) {
            log::info!("P2P[{}]: draining buffered offer", hex::encode(&peer[..4]));
            let _ = tx.send(buffered);
        } else {
            log::debug!("P2P[{}]: buffered offer expired, dropped", hex::encode(&peer[..4]));
        }
    }
    (rx, tx)
}

/// Route an inbound candidate offer to the session listening for `from`,
/// or buffer it and start a session that answers it. A stale offer names
/// a bridge nobody waits on any more and is dropped on the spot.
pub fn deliver(from: [u8; 32], mut offer: Offer) {
    offer.candidates.retain(super::punch::is_punchable);
    offer.candidates.truncate(super::punch::MAX_CANDIDATES);
    if !offer.is_fresh(now_ms()) {
        log::info!("P2P[{}]: offer expired in transit, dropped", hex::encode(&from[..4]));
        return;
    }
    let listener = LISTENERS.lock().get(&from).cloned();
    match listener {
        // Routed to the waiting session; `quic/server.rs` already logged arrival.
        Some(tx) if tx.send(offer.clone()).is_ok() => {},
        _ => {
            if matches!(crate::p2p::consent::may_connect(&from), crate::p2p::consent::Decision::No)
            {
                log::info!("P2P[{}]: offer denied by consent", hex::encode(&from[..4]));
                return;
            }
            // Disclosure is reciprocal: a peer who showed us their addresses
            // gets ours and a punch; one who offered only a bridge gets a
            // bridge and nothing to harvest.
            let disclosure = if offer.candidates.is_empty() {
                crate::p2p::Disclosure::RelayOnly
            } else {
                crate::p2p::Disclosure::Direct
            };
            log::info!(
                "P2P[{}]: offer arrived with no waiting session ({} cands) — answering {}",
                hex::encode(&from[..4]),
                offer.candidates.len(),
                if disclosure == crate::p2p::Disclosure::Direct { "direct" } else { "relayed" }
            );
            let session = offer.session;
            PENDING.lock().insert(from, offer);
            crate::RUNTIME.spawn(async move {
                let r = crate::p2p::connect_with(from, disclosure, Some(session)).await;
                if let Err(e) = r {
                    log::debug!("P2P[{}]: answer ended — {e}", hex::encode(&from[..4]));
                }
            });
        },
    }
}

/// Stop listening for `peer`'s offers, if `tx` is still the listener on
/// record. A newer session for the same peer keeps its own.
pub fn stop(peer: [u8; 32], tx: &mpsc::UnboundedSender<Offer>) {
    let mut listeners = LISTENERS.lock();
    if listeners.get(&peer).is_some_and(|t| t.same_channel(tx)) {
        listeners.remove(&peer);
    }
}

/// Send our candidate addresses (home relay + bridge token, for TURN) to
/// `peer` over the MLS channel.
pub async fn send_offer(peer: [u8; 32], offer: &Offer) -> Result<()> {
    log::info!(
        "P2P[{}]: sending offer — {} cands [{}], relay {}{}",
        hex::encode(&peer[..4]),
        offer.candidates.len(),
        addrs_short(&offer.candidates),
        offer.relay.map(addr_short).unwrap_or_else(|| "none".into()),
        if offer.in_reply_to.is_some() { " (reply)" } else { "" },
    );
    // Signalling is inherently point-to-point: a candidate path is between
    // two devices, so it addresses the direct conversation with that peer
    // rather than whatever group chat they happen to share with us.
    let conversation = crate::data::conversation::Conversation::for_peer(&peer)?;
    crate::messaging::send_control(
        conversation,
        AppPayload::P2pOffer {
            session:       offer.session,
            in_reply_to:   offer.in_reply_to,
            expires_at_ms: offer.expires_at_ms,
            candidates:    offer.candidates.clone(),
            relay:         offer.relay,
            token:         offer.token,
            disco_key:     offer.disco_key,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(session: u8, in_reply_to: Option<u8>, expires_at_ms: u64) -> Offer {
        Offer {
            session: [session; 16],
            in_reply_to: in_reply_to.map(|b| [b; 16]),
            expires_at_ms,
            candidates: vec!["9.9.9.9:9000".parse().unwrap()],
            relay: Some("5.5.5.5:443".parse().unwrap()),
            token: [7; 16],
            disco_key: [9; 32],
        }
    }

    #[test]
    fn deliver_routes_to_listener_by_ipk() {
        let peer = [42u8; 32];
        let (mut rx, tx) = listen(peer);

        deliver(peer, offer(1, None, now_ms() + 10_000));
        assert_eq!(rx.try_recv().unwrap().session, [1; 16]);
        stop(peer, &tx);
    }

    #[test]
    fn deliver_drops_unroutable_and_caps_candidates() {
        let peer = [44u8; 32];
        let (mut rx, tx) = listen(peer);

        let mut cands: Vec<SocketAddr> = vec![
            "127.0.0.1:5000".parse().unwrap(),
            "224.0.0.1:5000".parse().unwrap(),
        ];
        cands.extend((0..64u16).map(|i| SocketAddr::from(([9, 9, 9, 9], 5000 + i))));
        let mut o = offer(1, None, now_ms() + 10_000);
        o.candidates = cands;
        deliver(peer, o);

        let got = rx.try_recv().unwrap().candidates;
        assert_eq!(got.len(), super::super::punch::MAX_CANDIDATES);
        assert!(got.iter().all(|a| a.ip() == "9.9.9.9".parse::<std::net::IpAddr>().unwrap()));
        stop(peer, &tx);
    }

    #[test]
    fn offer_before_listener_is_buffered_then_drained() {
        use crate::data::contact::Contact;
        let peer = [43u8; 32];
        // The consent gate discards offers from unpaired contacts, so pair the
        // source before delivering — otherwise the offer never buffers.
        Contact::save_pending(peer, "peer".into()).unwrap();
        Contact::mark_paired(&peer);
        // arrives before anyone listens → buffered, no panic
        deliver(peer, offer(3, None, now_ms() + 10_000));
        // the late session still gets it, relay + secrets included
        let (mut rx, tx) = listen(peer);
        let got = rx.try_recv().unwrap();
        assert_eq!(got.session, [3; 16]);
        assert_eq!(got.relay, Some("5.5.5.5:443".parse().unwrap()));
        assert_eq!(got.token, [7; 16]);
        assert_eq!(got.disco_key, [9; 32]);
        stop(peer, &tx);
        let _ = Contact::delete(&peer);
    }

    /// A queued offer that outlived its bridge must not spend a session:
    /// dropped on delivery, and dropped from the buffer if it aged there.
    #[test]
    fn stale_offers_are_dropped_on_delivery_and_in_the_buffer() {
        let peer = [45u8; 32];
        let (mut rx, tx) = listen(peer);
        deliver(peer, offer(1, None, now_ms() - CLOCK_SKEW_MS - 1_000));
        assert!(rx.try_recv().is_err(), "stale offer must not reach the session");
        stop(peer, &tx);

        PENDING.lock().insert(peer, offer(2, None, now_ms() - CLOCK_SKEW_MS - 1_000));
        let (mut rx, tx) = listen(peer);
        assert!(rx.try_recv().is_err(), "stale buffered offer must not drain");
        stop(peer, &tx);
    }

    #[test]
    fn matching_pairs_replies_with_their_session_only() {
        let now = now_ms();
        let mine = [1u8; 16];
        assert!(offer(2, None, now + 1).matches(mine, None, now), "unsolicited pairs with anyone");
        assert!(offer(2, Some(1), now + 1).matches(mine, None, now), "a reply to us pairs");
        assert!(!offer(2, Some(9), now + 1).matches(mine, None, now), "a reply to another attempt does not");
        assert!(offer(2, Some(9), now + 1).matches(mine, Some([2; 16]), now), "unless it is the offer we answer");
        assert!(!offer(2, None, now - CLOCK_SKEW_MS - 1).matches(mine, None, now), "never when stale");
    }

    /// An old session's teardown must not unhook the session that replaced it.
    #[test]
    fn stop_only_removes_its_own_listener() {
        let peer = [46u8; 32];
        let (_old_rx, old_tx) = listen(peer);
        let (mut new_rx, new_tx) = listen(peer);
        stop(peer, &old_tx);
        deliver(peer, offer(1, None, now_ms() + 10_000));
        assert!(new_rx.try_recv().is_ok(), "the newer listener still receives");
        stop(peer, &new_tx);
    }
}
