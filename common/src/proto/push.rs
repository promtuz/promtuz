//! Push wire types: device-to-gateway registration and relay-to-gateway wake. The gateway holds
//! `P → token` and relays hold `IPK → P`, so neither alone links a user to a device.

use serde::Deserialize;
use serde::Serialize;

use crate::proto::client_rel::Wake;
use crate::proto::client_res::GatewayDescriptor;
use crate::proto::pack::bounded_vec;
use crate::types::bytes::Bytes;

/// Keeps a registration signature from being lifted into another protocol context.
const REGISTER_DOMAIN: &[u8] = b"promtuz-push-register-v1";

pub const MAX_PUSH_TOKEN_BYTES: usize = 512;

/// FCM's own limit on a data message.
pub const MAX_WAKE_PAYLOAD_BYTES: usize = 4096;

/// Registration candidates must match the relay's bounded wake fanout.
pub const MAX_PUSH_GATEWAYS: usize = 8;

/// The gateways a relay wakes and a device registers with: the [`MAX_PUSH_GATEWAYS`] lowest ids
/// among the verified ones. Both sides pick the same set, so a registration lands where wakes go.
/// Capping before verifying would let unverified low ids crowd out every real gateway.
pub fn wake_targets(
    mut directory: Vec<GatewayDescriptor>, verified: impl FnMut(&GatewayDescriptor) -> bool,
) -> Vec<GatewayDescriptor> {
    directory.retain(verified);
    directory.sort_by_key(|gateway| gateway.id);
    directory.truncate(MAX_PUSH_GATEWAYS);
    directory
}

/// Sent only after the gateway commits a device's registration.
#[derive(Debug, Serialize, Deserialize)]
pub enum RegisterResponse {
    Registered,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushProvider {
    Fcm,
    Apns,
    UnifiedPush,
}

impl PushProvider {
    /// Signing-input byte: never renumber, only append.
    fn tag(self) -> u8 {
        match self {
            PushProvider::Fcm => 0,
            PushProvider::Apns => 1,
            PushProvider::UnifiedPush => 2,
        }
    }
}

/// Binds the signature to `(provider, token)`, so a captured one cannot move to another token.
pub fn register_signing_input(provider: PushProvider, token: &[u8]) -> Vec<u8> {
    [REGISTER_DOMAIN, &[provider.tag()], token].concat()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterToken {
    /// Per-install pseudonym `P`: a random Ed25519 key unrelated to the IPK, which verifies `sig`.
    pub pseudonym: Bytes<32>,
    pub provider:  PushProvider,
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_PUSH_TOKEN_BYTES>")]
    pub token:     Vec<u8>,
    pub sig:       Bytes<64>,
}

/// Sent only by a home relay holding the recipient's queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeRequest {
    pub pseudonym: Bytes<32>,
    /// Reserved payload field. Current gateways accept only an empty wake;
    /// message content is fetched from the relay.
    #[serde(deserialize_with = "bounded_vec::<_, _, MAX_WAKE_PAYLOAD_BYTES>")]
    pub payload:   Vec<u8>,
    /// What is waiting: a call rides the platform's highest priority with a
    /// short life, a message the ordinary one. Never [`Wake::No`].
    pub class:     Wake,
}

/// One per bi-stream. `Register` and `Store` arrive over `client/N`, `Wake` over `relay/N` with
/// no reply. Append variants, never reorder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GatewayRequest {
    Register(RegisterToken),
    Wake(WakeRequest),
    Store(crate::proto::sticker::StoreRequest),
}

#[cfg(feature = "crypto")]
impl RegisterToken {
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

    pub fn verify(&self) -> bool {
        self.token.len() <= MAX_PUSH_TOKEN_BYTES
            && crate::crypto::verify_ed25519(
                &self.pseudonym.0,
                &register_signing_input(self.provider, &self.token),
                &self.sig.0,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::RelayId;

    #[test]
    fn wake_targets_cap_only_verified_gateways() {
        let directory: Vec<GatewayDescriptor> = (0..20u8)
            .rev()
            .map(|n| GatewayDescriptor {
                id:     RelayId::from_bytes([n; 32]),
                addr:   "127.0.0.1:1".parse().unwrap(),
                pubkey: Bytes([n; 32]),
            })
            .collect();
        let first = |gateway: &GatewayDescriptor| gateway.id.as_bytes()[0];
        for (verified, expected) in [(0..20, 0..8), (10..20, 10..18)] {
            let targets = wake_targets(directory.clone(), |g| verified.contains(&first(g)));
            assert_eq!(targets.iter().map(first).collect::<Vec<_>>(), expected.collect::<Vec<_>>());
        }
    }

    #[test]
    fn transcripts() {
        crate::proto::golden(
            &[
                register_signing_input(PushProvider::UnifiedPush, b"token"),
            ],
            "0598fa6a183dc468a00ece6020b42b7842533097fd01a87222a0d9e8a117cdb0",
        );
    }
}
