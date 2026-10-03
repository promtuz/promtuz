//! Hole-punch pokes on the QUIC socket itself, framed `MAGIC | channel | nonce | AEAD(msg)`. The
//! key travels in the MLS-sealed candidate offer, so no one outside the group can forge a poke.

use std::net::SocketAddr;

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use serde::Deserialize;
use serde::Serialize;

/// Its first byte keeps the QUIC fixed bit (`0x40`) clear, so a stray poke never parses as QUIC.
const MAGIC: [u8; 4] = [0x2e, 0x70, 0x32, 0x70]; // ".p2p"
const CHAN_LEN: usize = 8;
const NONCE_LEN: usize = 24;
const HEADER_LEN: usize = MAGIC.len() + CHAN_LEN + NONCE_LEN;

/// `Pong` echoes where the pinger was seen from, which doubles as reflexive address discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoMsg {
    Ping { tx: [u8; 8] },
    Pong { tx: [u8; 8], seen: SocketAddr },
}

/// `channel` is a public routing tag; only the key is secret.
pub struct DiscoKey {
    cipher: XChaCha20Poly1305,
    channel: [u8; CHAN_LEN],
}

impl DiscoKey {
    pub fn new(key: &[u8; 32], channel: [u8; CHAN_LEN]) -> Self {
        Self { cipher: XChaCha20Poly1305::new(key.into()), channel }
    }

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

pub fn peek_channel(pkt: &[u8]) -> Option<[u8; CHAN_LEN]> {
    if pkt.len() < HEADER_LEN || !pkt.starts_with(&MAGIC) {
        return None;
    }
    let mut chan = [0u8; CHAN_LEN];
    chan.copy_from_slice(&pkt[MAGIC.len()..MAGIC.len() + CHAN_LEN]);
    Some(chan)
}
