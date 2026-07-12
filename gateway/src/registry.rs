use std::collections::HashMap;

use common::proto::push::PushProvider;
use common::proto::push::RegisterToken;
use parking_lot::RwLock;

/// A stored device wake target under a pseudonym `P`.
#[derive(Debug, Clone)]
pub struct TokenEntry {
    pub provider: PushProvider,
    pub token:    Vec<u8>,
}

/// The `P → token` registry. The gateway learns the token only under the
/// pseudonym `P`; it never sees the IPK.
///
// ponytail: in-memory. A gateway restart drops registrations until devices
// re-register (which they do on next foreground). Persist to a small on-disk
// KV only if that window ever proves to matter.
#[derive(Default)]
pub struct PushRegistry {
    map: RwLock<HashMap<[u8; 32], TokenEntry>>,
}

impl PushRegistry {
    /// Verify a self-signed registration, then store `P → token`
    /// (last-write-wins, so a rotated token just overwrites the old one).
    /// Rejects a bad signature — the gateway must not store a target it can't
    /// attribute to the holder of `P`.
    pub fn register(&self, reg: &RegisterToken) -> Result<(), &'static str> {
        if !reg.verify() {
            return Err("bad registration signature");
        }
        self.map.write().insert(
            reg.pseudonym.0,
            TokenEntry { provider: reg.provider, token: reg.token.clone() },
        );
        Ok(())
    }

    /// Look up a pseudonym's current wake target (for a `WakeRequest`).
    pub fn resolve(&self, pseudonym: &[u8; 32]) -> Option<TokenEntry> {
        self.map.read().get(pseudonym).cloned()
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    #[test]
    fn register_then_resolve() {
        let reg = RegisterToken::signed(
            &SigningKey::from_bytes(&[7u8; 32]),
            PushProvider::Fcm,
            b"tok".to_vec(),
        );
        let p = reg.pseudonym.0;
        let registry = PushRegistry::default();
        assert!(registry.register(&reg).is_ok());
        assert_eq!(registry.resolve(&p).unwrap().token, b"tok");
    }

    #[test]
    fn rejects_bad_signature() {
        let mut reg = RegisterToken::signed(
            &SigningKey::from_bytes(&[7u8; 32]),
            PushProvider::Fcm,
            b"tok".to_vec(),
        );
        reg.token = b"evil".to_vec(); // signature no longer matches the token
        assert!(PushRegistry::default().register(&reg).is_err());
    }
}
