use anyhow::Result;
use ed25519_dalek::VerifyingKey;

use crate::data::identity::Identity;

pub struct PeerIdentity {
    pub public_key: VerifyingKey,
}

impl PeerIdentity {
    pub fn initialize() -> Result<Self> {
        let public_key = VerifyingKey::from_bytes(&Identity::public_key()?.to_bytes())?;

        Ok(Self { public_key })
    }
}
