use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

pub use ed25519_dalek::SigningKey;

/// HKDF info for the P2P TLS sub-key. Changing it rotates the key contacts expect in the peer
/// cert's SPKI.
pub const P2P_TLS_INFO: &[u8] = b"promtuz-p2p-tls-v1";

/// A TLS-only sub-key, so rustls never signs with the identity key. The identity public key is
/// the HKDF salt, binding the sub-key to this user.
pub fn derive_p2p_tls_key(
    identity_secret: &[u8; 32], identity_public: &[u8; 32],
) -> SigningKey {
    let hkdf = Hkdf::<Sha256>::new(Some(identity_public), identity_secret);
    let mut seed = Zeroizing::new([0u8; 32]);
    hkdf.expand(P2P_TLS_INFO, seed.as_mut())
        .expect("HKDF-SHA256 expand into 32 bytes never fails");
    SigningKey::from_bytes(&seed)
}
