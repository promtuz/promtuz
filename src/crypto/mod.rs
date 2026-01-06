use chacha20poly1305::aead::{OsRng, rand_core::RngCore};

#[cfg(feature = "sign")]
pub use ed25519_dalek::{SecretKey, SigningKey, VerifyingKey as PublicKey};

use hkdf::Hkdf;

use sha2::Sha256;
pub use x25519_dalek::{EphemeralSecret, SharedSecret, StaticSecret, PublicKey as xPublicKey};

pub mod encrypt;

#[cfg(feature = "sign")]
pub mod sign;

pub fn get_secret_key() -> StaticSecret {
    StaticSecret::random_from_rng(OsRng)
}

pub fn derive_public_key(key: &StaticSecret) -> x25519_dalek::PublicKey {
    x25519_dalek::PublicKey::from(key)
}

pub fn get_ephemeral_keypair() -> (EphemeralSecret, x25519_dalek::PublicKey) {
    let esk = EphemeralSecret::random_from_rng(OsRng);
    let epk = x25519_dalek::PublicKey::from(&esk);

    (esk, epk)
}

pub fn get_static_keypair() -> (StaticSecret, x25519_dalek::PublicKey) {
    let esk = StaticSecret::random_from_rng(OsRng);
    let epk = x25519_dalek::PublicKey::from(&esk);

    (esk, epk)
}

#[cfg(feature = "sign")]
pub fn get_signing_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

pub fn get_shared_key(shared_secret: &[u8; 32], salt: &[u8], info: &str) -> [u8; 32] {
    let key = Hkdf::<Sha256>::new(Some(salt), shared_secret);
    let mut okm = [0u8; 32];
    let _ = key.expand(info.as_bytes(), &mut okm);

    okm
}

pub fn get_nonce<const N: usize>() -> [u8; N] {
    let mut nonce = [0u8; N];
    OsRng.fill_bytes(&mut nonce);
    nonce
}
