use hkdf::Hkdf;
use sha2::Sha256;

pub use ed25519_dalek::SigningKey;

/// Signature key is derived from identity key using this value
static SIGNATURE_MAGIC: &[u8; 12] = b"\x80\x14\x3b\x55\xfa\xf7\xda\xaf\xfb\xab\x66\x89";

pub fn derive_ed25519(private: &[u8; 32]) -> SigningKey {
    let hkdf = Hkdf::<Sha256>::new(None, private);
    let mut output = [0u8; 32];

    _ = hkdf.expand(SIGNATURE_MAGIC, &mut output);

    SigningKey::from_bytes(&output)
}