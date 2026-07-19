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

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Result;
use common::proto::mls_wire::AppPayload;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// A peer's connection offer: where to reach them directly, their home relay
/// for the TURN fallback, and random session secrets — a bridge token and a
/// disco key (the dialer's win).
#[derive(Debug, Clone)]
pub struct Offer {
    pub candidates: Vec<SocketAddr>,
    pub relay:      Option<SocketAddr>,
    pub token:      [u8; 16],
    pub disco_key:  [u8; 32],
}

/// Peer IPK → the live session waiting for that peer's candidate offer.
type Listeners = Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<Offer>>>;
static LISTENERS: Lazy<Listeners> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Offers that arrived before their session was listening. Best-effort,
/// no TTL — the next [`listen`] drains it, [`stop`] clears it. Fine for
/// the near-simultaneous connect the transport does; a real freshness
/// bound comes with the wake-rendezvous later.
static PENDING: Lazy<Mutex<HashMap<[u8; 32], Offer>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Start listening for `peer`'s candidate offers. Returns the receiver;
/// any offer that already arrived is delivered immediately.
pub fn listen(peer: [u8; 32]) -> mpsc::UnboundedReceiver<Offer> {
    let (tx, rx) = mpsc::unbounded_channel();
    LISTENERS.lock().insert(peer, tx.clone());
    if let Some(buffered) = PENDING.lock().remove(&peer) {
        log::info!("P2P: draining buffered offer from {}", hex::encode(&peer[..4]));
        let _ = tx.send(buffered);
    }
    rx
}

/// Route an inbound candidate offer to the session listening for `from`,
/// or buffer it for a session that registers momentarily later.
pub fn deliver(
    from: [u8; 32], candidates: Vec<SocketAddr>, relay: Option<SocketAddr>, token: [u8; 16],
    disco_key: [u8; 32],
) {
    let offer = Offer { candidates, relay, token, disco_key };
    let listener = LISTENERS.lock().get(&from).cloned();
    match listener {
        Some(tx) if tx.send(offer.clone()).is_ok() => {
            log::info!(
                "P2P: delivered offer from {} ({} candidates) to session",
                hex::encode(&from[..4]),
                offer.candidates.len()
            );
        },
        _ => {
            let listening: Vec<String> =
                LISTENERS.lock().keys().map(|k| hex::encode(&k[..4])).collect();
            log::info!(
                "P2P: offer from {} ({} candidates), no session (listening for {:?}) — \
                 buffering + auto-accepting",
                hex::encode(&from[..4]),
                offer.candidates.len(),
                listening
            );
            PENDING.lock().insert(from, offer);
            // Auto-accept: the peer wants a direct link and we're not already
            // reaching for them, so start a session. It drains the buffered
            // offer and answers. (Consent gate — may_connect — comes later.)
            crate::RUNTIME.spawn(async move {
                match crate::p2p::connect(from).await {
                    Ok(_) => log::info!("P2P: auto-accepted {}", hex::encode(&from[..4])),
                    Err(e) => log::info!("P2P: auto-accept {} ended: {e}", hex::encode(&from[..4])),
                }
            });
        },
    }
}

/// Stop listening for `peer`'s offers — the session ended.
pub fn stop(peer: [u8; 32]) {
    LISTENERS.lock().remove(&peer);
    PENDING.lock().remove(&peer);
}

/// Send our candidate addresses (home relay + bridge token, for TURN) to
/// `peer` over the MLS channel.
pub async fn send_offer(
    peer: [u8; 32], candidates: Vec<SocketAddr>, relay: Option<SocketAddr>, token: [u8; 16],
    disco_key: [u8; 32],
) -> Result<()> {
    log::info!(
        "P2P: sending offer to {} ({} candidates: {:?}, relay {:?})",
        hex::encode(&peer[..4]),
        candidates.len(),
        candidates,
        relay
    );
    crate::messaging::send_control(peer, AppPayload::P2p { candidates, relay, token, disco_key })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_routes_to_listener_by_ipk() {
        let peer = [42u8; 32];
        let mut rx = listen(peer);

        let cands: Vec<SocketAddr> = vec!["1.2.3.4:5".parse().unwrap()];
        deliver(peer, cands.clone(), None, [0; 16], [0; 32]);
        assert_eq!(rx.try_recv().unwrap().candidates, cands);
        stop(peer);
    }

    #[test]
    fn offer_before_listener_is_buffered_then_drained() {
        let peer = [43u8; 32];
        let cands: Vec<SocketAddr> = vec!["9.9.9.9:9".parse().unwrap()];
        let relay: SocketAddr = "5.5.5.5:443".parse().unwrap();
        // arrives before anyone listens → buffered, no panic
        deliver(peer, cands.clone(), Some(relay), [7; 16], [9; 32]);
        // the late session still gets it, relay + secrets included
        let mut rx = listen(peer);
        let got = rx.try_recv().unwrap();
        assert_eq!(got.candidates, cands);
        assert_eq!(got.relay, Some(relay));
        assert_eq!(got.token, [7; 16]);
        assert_eq!(got.disco_key, [9; 32]);
        stop(peer);
    }
}
