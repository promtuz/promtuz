use anyhow::Result;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use jni::JNIEnv;

use crate::data::identity::Identity;

pub struct PeerIdentity {
    pub public_key: VerifyingKey,
}

impl PeerIdentity {
    pub fn initialize(env: &mut JNIEnv) -> Result<Self> {
        let isk = SigningKey::from_bytes(&*Identity::secret_key(env)?);
        let public_key = isk.verifying_key();

        Ok(Self { public_key })
    }
}
