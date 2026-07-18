//! P2P signaling: trade candidate addresses over the existing MLS
//! channel — the peer-to-peer analogue of "here's where to reach me",
//! but E2E and authenticated for free, so it replaces a relay-carried
//! call-me-maybe.
//!
//! A session started by the transport calls [`listen`] for the peer's
//! offer and [`send_offer`] to publish its own; the inbound MLS dispatch
//! (`quic/server.rs`) routes the peer's offer here via [`deliver`], keyed
//! by peer IPK.

use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::Result;
use common::proto::mls_wire::AppPayload;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::mpsc;

/// Peer IPK → the live session waiting for that peer's candidate offer.
static LISTENERS: Lazy<Mutex<HashMap<[u8; 32], mpsc::UnboundedSender<Vec<SocketAddr>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Start listening for `peer`'s candidate offers. Returns the receiver;
/// a later offer with no live receiver deregisters the slot.
pub fn listen(peer: [u8; 32]) -> mpsc::UnboundedReceiver<Vec<SocketAddr>> {
    let (tx, rx) = mpsc::unbounded_channel();
    LISTENERS.lock().insert(peer, tx);
    rx
}

/// Route an inbound candidate offer to the session listening for `from`.
/// Dropped if no session is active — we aren't trying to reach that peer.
pub fn deliver(from: [u8; 32], candidates: Vec<SocketAddr>) {
    let mut map = LISTENERS.lock();
    if let Some(tx) = map.get(&from)
        && tx.send(candidates).is_err()
    {
        map.remove(&from); // receiver dropped — session ended
    }
}

/// Send our candidate addresses to `peer` over the MLS channel.
pub async fn send_offer(peer: [u8; 32], candidates: Vec<SocketAddr>) -> Result<()> {
    crate::messaging::send_control(peer, AppPayload::P2p { candidates }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliver_routes_to_listener_by_ipk() {
        let peer = [42u8; 32];
        let mut rx = listen(peer);

        let cands: Vec<SocketAddr> = vec!["1.2.3.4:5".parse().unwrap()];
        deliver(peer, cands.clone());
        assert_eq!(rx.try_recv().unwrap(), cands);

        // an offer for a peer with no listener is dropped, not a panic
        deliver([7u8; 32], vec!["9.9.9.9:9".parse().unwrap()]);
    }
}
