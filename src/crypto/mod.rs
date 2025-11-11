use chacha20poly1305::aead::{OsRng, rand_core::RngCore};

use hkdf::Hkdf;

use sha2::Sha256;
pub use x25519_dalek::{EphemeralSecret, PublicKey, SharedSecret, StaticSecret};
pub mod encrypt;

pub fn get_secret_key() -> StaticSecret {
    StaticSecret::random_from_rng(OsRng)
}

pub fn derive_public_key(key: &StaticSecret) -> PublicKey {
    PublicKey::from(key)
}

pub fn get_ephemeral_keypair() -> (EphemeralSecret, PublicKey) {
    let esk = EphemeralSecret::random_from_rng(OsRng);
    let epk = PublicKey::from(&esk);

    (esk, epk)
}

pub fn get_static_keypair() -> (StaticSecret, PublicKey) {
    let esk = StaticSecret::random_from_rng(OsRng);
    let epk = PublicKey::from(&esk);

    (esk, epk)
}

pub fn get_shared_key(shared_secret: &[u8; 32], salt: &str, info: &str) -> [u8; 32] {
    let key = Hkdf::<Sha256>::new(Some(salt.as_bytes()), shared_secret);
    let mut okm = [0u8; 32];
    let _ = key.expand(info.as_bytes(), &mut okm);

    okm
}

pub fn get_nonce<const N: usize>() -> [u8; N] {
    let mut nonce = [0u8; N];
    OsRng.fill_bytes(&mut nonce);
    nonce
}
