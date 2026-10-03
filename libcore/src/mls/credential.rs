//! What ties an MLS leaf to a promtuz identity. A leaf signs with its own key, so a leaf compromise
//! is not an identity compromise, and its credential `ipk ‖ Sig_ipk(DOMAIN ‖ leaf signature key)`
//! proves which identity chose that key. Without the proof any member could claim any IPK.
//!
//! Leaves minted before the binding carry the bare 32-byte IPK. Only a pair group accepts them,
//! and a pair Welcome must seat exactly its signed sender and recipient.

use common::crypto::verify_ed25519;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use openmls::prelude::BasicCredential;
use openmls::prelude::Credential;
use openmls::prelude::LeafNode;
use openmls::prelude::Member;

const DOMAIN: &[u8] = b"promtuz-mls-leaf-v1";
const BOUND_LEN: usize = 32 + 64;

fn binding_input(leaf_signature_key: &[u8]) -> Vec<u8> {
    [DOMAIN, leaf_signature_key].concat()
}

pub fn bound_credential(ipk_signer: &SigningKey, leaf_signature_key: &[u8]) -> BasicCredential {
    let sig = ipk_signer.sign(&binding_input(leaf_signature_key));
    let mut bytes = Vec::with_capacity(BOUND_LEN);
    bytes.extend_from_slice(&ipk_signer.verifying_key().to_bytes());
    bytes.extend_from_slice(&sig.to_bytes());
    BasicCredential::new(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafIdentity {
    /// Signed over the leaf's own signature key.
    Bound([u8; 32]),
    /// The bare IPK of a leaf minted before the binding existed.
    Legacy([u8; 32]),
}

impl LeafIdentity {
    pub fn ipk(self) -> [u8; 32] {
        match self {
            Self::Bound(k) | Self::Legacy(k) => k,
        }
    }

    /// Under `strict`, only a bound leaf is anyone.
    pub fn ipk_if(self, strict: bool) -> Option<[u8; 32]> {
        match self {
            Self::Bound(k) => Some(k),
            Self::Legacy(k) if !strict => Some(k),
            Self::Legacy(_) => None,
        }
    }
}

/// `None` unless the credential is a valid binding for this leaf key or the bare legacy form.
pub fn leaf_identity(credential: &Credential, leaf_signature_key: &[u8]) -> Option<LeafIdentity> {
    let bytes = credential.serialized_content();
    match bytes.len() {
        32 => Some(LeafIdentity::Legacy(bytes.try_into().ok()?)),
        BOUND_LEN => {
            let ipk: [u8; 32] = bytes[..32].try_into().ok()?;
            let sig = bytes[32..].try_into().ok()?;
            verify_ed25519(&ipk, &binding_input(leaf_signature_key), sig).ok()?;
            Some(LeafIdentity::Bound(ipk))
        },
        _ => None,
    }
}

pub fn member_ipk(m: &Member, strict: bool) -> Option<[u8; 32]> {
    leaf_identity(&m.credential, &m.signature_key)?.ipk_if(strict)
}

pub fn leaf_node_ipk(leaf: &LeafNode, strict: bool) -> Option<[u8; 32]> {
    leaf_identity(leaf.credential(), leaf.signature_key().as_slice())?.ipk_if(strict)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential naming Alice without her signature over that leaf key is nobody; her bare
    /// legacy form is nobody in a group but still the peer in a pair.
    #[test]
    fn a_claimed_ipk_without_her_signature_is_nobody() {
        let alice = SigningKey::from_bytes(&[1; 32]);
        let mallory = SigningKey::from_bytes(&[2; 32]);
        let ipk = alice.verifying_key().to_bytes();
        let leaf = [7; 32];
        let bound: Credential = bound_credential(&alice, &leaf).into();
        assert_eq!(leaf_identity(&bound, &leaf), Some(LeafIdentity::Bound(ipk)));
        assert_eq!(leaf_identity(&bound, &[8; 32]), None, "another leaf key is not hers");

        let mut claimed = ipk.to_vec();
        claimed.extend_from_slice(&mallory.sign(&binding_input(&leaf)).to_bytes());
        assert_eq!(leaf_identity(&BasicCredential::new(claimed).into(), &leaf), None);

        let bare = leaf_identity(&BasicCredential::new(ipk.to_vec()).into(), &leaf).unwrap();
        assert_eq!((bare.ipk_if(true), bare.ipk_if(false)), (None, Some(ipk)));
    }
}
