//! Push wire types: device → gateway registration and relay → gateway wake.
//! The gateway holds `P → token`; relays hold `IPK → P` — neither alone links
//! a user to a wakeable device.
//!
//! **Pseudonym `P` is a per-install Ed25519 public key** (random, unrelated to
//! the IPK). The device keeps the secret and self-signs each registration, so
//! the gateway can authenticate `P → token` without ever seeing the IPK.

use serde::Deserialize;
use serde::Serialize;

use crate::types::bytes::Bytes;

/// Domain tag mixed into the registration signing input so a signature can
/// never be lifted into another protocol context.
const REGISTER_DOMAIN: &[u8] = b"promtuz-push-register-v1";

/// Which platform wake service a token targets. The tag travels with every
/// registration so the gateway can add APNs / UnifiedPush as new dispatch arms
/// without a registry migration — the iOS-readiness pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushProvider {
    Fcm,
    Apns,
    UnifiedPush,
}

impl PushProvider {
    /// Stable byte for the signing input. Never renumber (would break existing
    /// signatures); append new providers.
    fn tag(self) -> u8 {
        match self {
            PushProvider::Fcm => 0,
            PushProvider::Apns => 1,
            PushProvider::UnifiedPush => 2,
        }
    }
}

/// The bytes a device signs to register `P → token`. Binds the signature to
/// `(provider, token)` so a captured signature can't be replayed onto a
/// different token.
pub fn register_signing_input(provider: PushProvider, token: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(REGISTER_DOMAIN.len() + 1 + token.len());
    v.extend_from_slice(REGISTER_DOMAIN);
    v.push(provider.tag());
    v.extend_from_slice(token);
    v
}

/// Device registers `P → token` with the gateway. Self-signed by `P` (see the
/// module note) so the gateway accepts only genuine registrations while never
/// seeing the IPK.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterToken {
    /// Per-install push pseudonym `P` — a random Ed25519 pubkey, not derivable
    /// from the IPK. Also the key that verifies `sig`.
    pub pseudonym: Bytes<32>,
    pub provider:  PushProvider,
    /// Opaque platform token (FCM registration token / APNs device token /
    /// UnifiedPush endpoint URL). Variable length.
    pub token:     Vec<u8>,
    /// Signature by `P` over `(provider, token)`.
    pub sig:       Bytes<64>,
}

/// A home relay asks the gateway to wake a device. Only a relay holding the
/// recipient's queue initiates this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeRequest {
    /// Recipient's push pseudonym `P` (the relay holds `IPK → P`).
    pub pseudonym: Bytes<32>,
    /// Wake payload: the queued MLS ciphertext envelope (≤ push size limit) or
    /// a contentless pointer. The gateway forwards it blind.
    pub payload:   Vec<u8>,
}

/// One-RPC-per-bi-stream request the gateway unpacks (mirrors the resolver's
/// `ClientRequest`). `Register` arrives over `client/1`, `Wake` over
/// `relay/1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PushRequest {
    Register(RegisterToken),
    Wake(WakeRequest),
}

#[cfg(feature = "crypto")]
impl RegisterToken {
    /// Build a self-signed registration. `P` is `key`'s public key — a
    /// per-install push keypair, unrelated to the IPK.
    pub fn signed(
        key: &ed25519_dalek::SigningKey, provider: PushProvider, token: Vec<u8>,
    ) -> Self {
        use ed25519_dalek::Signer;
        let sig = key.sign(&register_signing_input(provider, &token));
        Self {
            pseudonym: Bytes(key.verifying_key().to_bytes()),
            provider,
            token,
            sig: Bytes(sig.to_bytes()),
        }
    }

    /// Verify the self-signature: `sig` must be a valid signature by
    /// `pseudonym` (as an Ed25519 pubkey) over this registration. IPK-free.
    pub fn verify(&self) -> bool {
        use ed25519_dalek::Signature;
        use ed25519_dalek::VerifyingKey;
        let Ok(vk) = VerifyingKey::from_bytes(&self.pseudonym.0) else {
            return false;
        };
        let sig = Signature::from_bytes(&self.sig.0);
        vk.verify_strict(&register_signing_input(self.provider, &self.token), &sig).is_ok()
    }
}

#[cfg(all(test, feature = "crypto"))]
mod tests {
    use super::*;

    #[test]
    fn self_signed_registration_round_trips() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let reg = RegisterToken::signed(&key, PushProvider::Fcm, b"token-bytes".to_vec());
        assert!(reg.verify());
    }

    #[test]
    fn tampered_token_fails_verify() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut reg = RegisterToken::signed(&key, PushProvider::Fcm, b"token-bytes".to_vec());
        reg.token = b"evil-token".to_vec();
        assert!(!reg.verify());
    }
}
