use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

pub use ed25519_dalek::SigningKey;

/// Signature key is derived from identity key using this value
static SIGNATURE_MAGIC: &[u8; 12] = b"\x80\x14\x3b\x55\xfa\xf7\xda\xaf\xfb\xab\x66\x89";

pub fn derive_ed25519(private: &[u8; 32]) -> SigningKey {
    let hkdf = Hkdf::<Sha256>::new(None, private);
    let mut output = [0u8; 32];

    _ = hkdf.expand(SIGNATURE_MAGIC, &mut output);

    SigningKey::from_bytes(&output)
}

/// HKDF info for the per-user P2P TLS sub-key.
///
/// Bumping this domain string rotates the derived key. Keep it stable across
/// releases — the public bytes of the derived key are embedded in the SPKI
/// of the peer cert and any change here invalidates contacts' stored
/// expectations of that cert.
pub const P2P_TLS_INFO: &[u8] = b"promtuz-p2p-tls-v1";

/// Derive a stable, per-identity Ed25519 sub-key dedicated to TLS-layer
/// signing for peer-to-peer QUIC handshakes.
///
/// Why a sub-key? The TLS handshake signer (rustls) signs the rustls TLS 1.3
/// CertificateVerify transcript with whatever key we hand it. The rest of
/// the application also signs structured messages with the long-term
/// identity key (e.g. `dispatch_sig_message`). Even though the two
/// "messages" have different prefixes today, deterministic Ed25519 makes
/// any cross-protocol confusion permanent if it is ever discovered. Giving
/// TLS its own HKDF-derived sub-key turns "two contexts using one key" into
/// "one context per key" with zero extra ceremony — the sub-key is
/// derivable on demand from the long-term key, never persisted, and the
/// public component goes into the cert SPKI so the binding is verifiable.
///
/// `identity_secret` is the 32-byte Ed25519 seed; `identity_public` is the
/// matching public key, used as HKDF salt to make the derivation
/// per-identity (so the sub-key is bound to *this* user, not an opaque
/// global derivation that could collide across user databases on the same
/// device).
pub fn derive_p2p_tls_key(
    identity_secret: &[u8; 32], identity_public: &[u8; 32],
) -> SigningKey {
    let hkdf = Hkdf::<Sha256>::new(Some(identity_public), identity_secret);
    // The intermediate seed lives on the stack only as long as this scope —
    // wrapping it in `Zeroizing` makes that wipe explicit for readers and
    // guarantees no copy of the raw 32 bytes is left behind even on
    // optimization paths that would otherwise keep the buffer alive.
    let mut seed = Zeroizing::new([0u8; 32]);
    hkdf.expand(P2P_TLS_INFO, seed.as_mut())
        .expect("HKDF-SHA256 expand into 32 bytes never fails");
    // `SigningKey` itself implements `ZeroizeOnDrop` (via the `zeroize`
    // feature) so the returned value self-wipes when the caller drops it —
    // an outer `Zeroizing` wrapper would be redundant and force `Zeroize`
    // (not just `ZeroizeOnDrop`) on every caller.
    SigningKey::from_bytes(&seed)
}