use data_encoding::BASE32_NOPAD;
use p256::elliptic_curve::sec1::ToEncodedPoint;

/// Generate a compact, human-safe ID from a public key.
pub fn derive_id(pubkey: &p256::PublicKey) -> String {
    // Use SEC1 uncompressed bytes (standard, same as OpenSSL)
    let encoded = pubkey.to_encoded_point(false);
    let hash = blake3::hash(encoded.as_bytes());

    // Use first 10 bytes → ~16 chars of Base32 (short + unique)
    let short = &hash.as_bytes()[..10];
    BASE32_NOPAD.encode(short)
}