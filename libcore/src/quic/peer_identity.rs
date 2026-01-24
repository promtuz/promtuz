use std::sync::Arc;

use anyhow::Result;
use ed25519_dalek::VerifyingKey;
use jni::JNIEnv;

use crate::data::identity::Identity;
use crate::data::identity::IdentitySigner;

pub struct PeerIdentity {
    pub public_key: VerifyingKey,
    pub signer: Arc<IdentitySigner>,
}

impl PeerIdentity {
    pub fn initialize(env: &mut JNIEnv) -> Result<Self> {
        let public_key = VerifyingKey::from_bytes(&Identity::public_key()?.to_bytes())?;
        let signer = Arc::new(IdentitySigner::new(env)?);

        Ok(Self { public_key, signer })
    }
}
