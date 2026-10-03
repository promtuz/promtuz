//! P2P signaling: candidate offers over the MLS channel. Each offer expires, so a peer who was
//! away does not answer a queue of dead bridges.

use std::net::SocketAddr;

use anyhow::Result;
use common::proto::mls_wire::AppPayload;
use common::utils::now_ms;
use tokio::sync::mpsc;

use crate::state::core;
use crate::utils::addr_short;
use crate::utils::addrs_short;

const CLOCK_SKEW_MS: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct Offer {
    /// The sender's connect attempt this offer belongs to.
    pub session:       [u8; 16],
    pub in_reply_to:   Option<[u8; 16]>,
    /// On the sender's clock; past it the bridge and the secrets are dead.
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

    pub fn matches(&self, mine: [u8; 16], answering: Option<[u8; 16]>, now_ms: u64) -> bool {
        self.is_fresh(now_ms)
            && (self.in_reply_to.is_none()
                || self.in_reply_to == Some(mine)
                || answering == Some(self.session))
    }
}

/// The returned sender identifies this listener to [`stop`].
pub fn listen(peer: [u8; 32]) -> (mpsc::UnboundedReceiver<Offer>, mpsc::UnboundedSender<Offer>) {
    let (tx, rx) = mpsc::unbounded_channel();
    core().p2p.offer_listeners.lock().insert(peer, tx.clone());
    if let Some(buffered) = core().p2p.early_offers.lock().remove(&peer) {
        if buffered.is_fresh(now_ms()) {
            log::info!("P2P[{}]: draining buffered offer", hex::encode(&peer[..4]));
            let _ = tx.send(buffered);
        } else {
            log::debug!("P2P[{}]: buffered offer expired, dropped", hex::encode(&peer[..4]));
        }
    }
    (rx, tx)
}

pub fn deliver(from: [u8; 32], mut offer: Offer) {
    offer.candidates.retain(super::punch::is_punchable);
    offer.candidates.truncate(super::punch::MAX_CANDIDATES);
    if !offer.is_fresh(now_ms()) {
        log::info!("P2P[{}]: offer expired in transit, dropped", hex::encode(&from[..4]));
        return;
    }
    let listener = core().p2p.offer_listeners.lock().get(&from).cloned();
    match listener {
        Some(tx) if tx.send(offer.clone()).is_ok() => {},
        _ => {
            if matches!(crate::p2p::consent::may_connect(&from), crate::p2p::consent::Decision::No)
            {
                log::info!("P2P[{}]: offer denied by consent", hex::encode(&from[..4]));
                return;
            }
            // Disclosure is reciprocal: a peer who showed us their addresses gets ours; one who
            // offered only a bridge gets only a bridge.
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
            core().p2p.early_offers.lock().insert(from, offer);
            core().spawn(async move {
                let r = crate::p2p::connect_with(from, disclosure, Some(session)).await;
                if let Err(e) = r {
                    log::debug!("P2P[{}]: answer ended — {e}", hex::encode(&from[..4]));
                }
            });
        },
    }
}

/// A newer session for the same peer keeps its own listener.
pub fn stop(peer: [u8; 32], tx: &mpsc::UnboundedSender<Offer>) {
    let mut listeners = core().p2p.offer_listeners.lock();
    if listeners.get(&peer).is_some_and(|t| t.same_channel(tx)) {
        listeners.remove(&peer);
    }
}

pub async fn send_offer(peer: [u8; 32], offer: &Offer) -> Result<()> {
    log::info!(
        "P2P[{}]: sending offer — {} cands [{}], relay {}{}",
        hex::encode(&peer[..4]),
        offer.candidates.len(),
        addrs_short(&offer.candidates),
        offer.relay.map(addr_short).unwrap_or_else(|| "none".into()),
        if offer.in_reply_to.is_some() { " (reply)" } else { "" },
    );
    // Only this member, via a shared group when there is no paired direct chat; never a broadcast.
    let paired = crate::data::contact::Contact::is_paired(&peer);
    let conversation = crate::data::conversation::Conversation::for_peer_transport(&peer, paired)
        .ok_or_else(|| anyhow::anyhow!("no shared chat for peer signaling"))?;
    crate::messaging::send_control_to(
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
        peer,
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
            candidates: vec![],
            relay: None,
            token: [7; 16],
            disco_key: [9; 32],
        }
    }

    #[test]
    fn an_offer_pairs_only_with_its_own_session_and_never_when_stale() {
        let (now, mine) = (1_000_000, [1; 16]);
        let stale = now - CLOCK_SKEW_MS - 1;
        for (offer, answering, pairs, why) in [
            (offer(2, None, now + 1), None, true, "an unsolicited offer pairs"),
            (offer(2, Some(1), now + 1), None, true, "a reply to our attempt"),
            (offer(2, Some(9), now + 1), None, false, "a reply to another attempt"),
            (offer(2, Some(9), now + 1), Some([2; 16]), true, "the offer we are answering"),
            (offer(2, None, now - CLOCK_SKEW_MS), None, true, "within the clock skew"),
            (offer(2, None, stale), None, false, "never when stale"),
        ] {
            assert_eq!(offer.matches(mine, answering, now), pairs, "{why}");
        }
    }
}
