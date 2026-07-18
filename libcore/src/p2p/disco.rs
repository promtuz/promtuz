//! Disco — the out-of-band hole-punch poke.
//!
//! A small UDP packet that shares the P2P QUIC socket with quinn. Two
//! clients behind NAT trade authenticated Ping/Pong pokes over the very
//! socket quinn will later connect through, so the NAT mapping a poke
//! opens is the one the QUIC handshake reuses.
//!
//! Framing: `MAGIC | channel | nonce | AEAD(msg)`.
//! - `MAGIC` lets the socket split disco from QUIC on receive, and its
//!   first byte keeps the QUIC fixed-bit (`0x40`) clear so a stray poke
//!   is dropped by quinn when the endpoint sets `grease_quic_bit(false)`.
//! - `channel` picks which peer session's key opens the packet, with no
//!   trial decryption.
//! - the AEAD key is derived per session from the MLS group secret, so a
//!   poke can't be forged off the group — no separate key exchange.

use std::net::SocketAddr;

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use serde::Deserialize;
use serde::Serialize;

/// Tags a datagram as disco, not QUIC. First byte has the QUIC fixed-bit
/// (`0x40`) clear so a leaked poke never parses as a QUIC packet.
const MAGIC: [u8; 4] = [0x2e, 0x70, 0x32, 0x70]; // ".p2p"
const CHAN_LEN: usize = 8;
const NONCE_LEN: usize = 24; // XChaCha20-Poly1305
const HEADER_LEN: usize = MAGIC.len() + CHAN_LEN + NONCE_LEN;

/// One poke. `Ping` probes a candidate path (and opens our own NAT);
/// `Pong` confirms the path and echoes where the pinger was seen from —
/// server-reflexive address discovery for free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoMsg {
    Ping { tx: [u8; 8] },
    Pong { tx: [u8; 8], seen: SocketAddr },
}

/// Per-session seal/open. `channel` is a public routing tag; the key is
/// the secret half.
pub struct DiscoKey {
    cipher: XChaCha20Poly1305,
    channel: [u8; CHAN_LEN],
}

impl DiscoKey {
    pub fn new(key: &[u8; 32], channel: [u8; CHAN_LEN]) -> Self {
        Self { cipher: XChaCha20Poly1305::new(key.into()), channel }
    }

    pub fn channel(&self) -> [u8; CHAN_LEN] {
        self.channel
    }

    /// Frame and encrypt a poke.
    pub fn seal(&self, msg: &DiscoMsg) -> Vec<u8> {
        let plain = postcard::to_allocvec(msg).expect("disco encode is infallible");
        let mut nonce = [0u8; NONCE_LEN];
        {
            use ed25519_dalek::ed25519::signature::rand_core::OsRng;
            use ed25519_dalek::ed25519::signature::rand_core::RngCore;
            OsRng.fill_bytes(&mut nonce);
        }
        let ct = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plain.as_slice())
            .expect("disco seal is infallible for a small plaintext");
        let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.channel);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// Open a poke framed for this session, or `None` if it isn't ours or
    /// fails authentication.
    pub fn open(&self, pkt: &[u8]) -> Option<DiscoMsg> {
        if peek_channel(pkt)? != self.channel {
            return None;
        }
        let nonce = &pkt[MAGIC.len() + CHAN_LEN..HEADER_LEN];
        let ct = &pkt[HEADER_LEN..];
        let plain = self.cipher.decrypt(XNonce::from_slice(nonce), ct).ok()?;
        postcard::from_bytes(&plain).ok()
    }
}

/// Is this a disco datagram, and if so which session? Lets the socket
/// route a poke to the right [`DiscoKey`] without holding any key.
pub fn peek_channel(pkt: &[u8]) -> Option<[u8; CHAN_LEN]> {
    if pkt.len() < HEADER_LEN || !pkt.starts_with(&MAGIC) {
        return None;
    }
    let mut chan = [0u8; CHAN_LEN];
    chan.copy_from_slice(&pkt[MAGIC.len()..MAGIC.len() + CHAN_LEN]);
    Some(chan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_reject() {
        let key = DiscoKey::new(&[7u8; 32], [1, 2, 3, 4, 5, 6, 7, 8]);
        let msg = DiscoMsg::Pong { tx: [9; 8], seen: "127.0.0.1:443".parse().unwrap() };
        let pkt = key.seal(&msg);

        // routes to our channel and opens to the same message
        assert_eq!(peek_channel(&pkt), Some([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(key.open(&pkt), Some(msg));

        // right channel, wrong key → authentication fails
        let wrong_key = DiscoKey::new(&[8u8; 32], [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(wrong_key.open(&pkt), None);

        // wrong channel → rejected before any decryption
        let wrong_chan = DiscoKey::new(&[7u8; 32], [0; 8]);
        assert_eq!(wrong_chan.open(&pkt), None);

        // a QUIC-shaped datagram (fixed-bit set) is not disco
        assert_eq!(peek_channel(&[0xc0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), None);
    }
}
