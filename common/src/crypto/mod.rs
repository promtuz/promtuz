pub use ed25519_dalek::SecretKey;
use ed25519_dalek::Signature;
use ed25519_dalek::SignatureError;
pub use ed25519_dalek::SigningKey;
pub use ed25519_dalek::VerifyingKey as PublicKey;
use rand::TryRng;
use rand::rngs::SysRng;
use zeroize::Zeroizing;

pub mod sign;

pub fn get_signing_key() -> SigningKey {
    let mut secret = Zeroizing::new(SecretKey::default());
    SysRng.try_fill_bytes(secret.as_mut()).expect("sysrng fail");
    SigningKey::from_bytes(&secret)
}

pub fn get_nonce<const N: usize>() -> [u8; N] {
    let mut nonce = [0u8; N];
    SysRng.try_fill_bytes(&mut nonce).expect("sysrng fail");
    nonce
}

pub fn verify_ed25519(pubkey: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> Result<(), SignatureError> {
    PublicKey::from_bytes(pubkey)?.verify_strict(msg, &Signature::from_bytes(sig))
}
