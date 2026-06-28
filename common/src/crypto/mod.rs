pub use ed25519_dalek::SecretKey;
pub use ed25519_dalek::SigningKey;
pub use ed25519_dalek::VerifyingKey as PublicKey;
use rand::TryRng;
use rand::rngs::SysRng;

pub mod sign;

pub fn get_signing_key() -> SigningKey {
    let mut secret = SecretKey::default();
    SysRng.try_fill_bytes(&mut secret).expect("sysrng fail");
    SigningKey::from_bytes(&secret)
}

pub fn get_nonce<const N: usize>() -> [u8; N] {
    let mut nonce = [0u8; N];
    SysRng.try_fill_bytes(&mut nonce).expect("sysrng fail");
    nonce
}
