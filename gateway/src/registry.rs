use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use common::proto::push::PushProvider;
use common::proto::push::RegisterToken;
use parking_lot::RwLock;

/// Ceiling on simultaneously-held registrations. Registration is
/// unauthenticated beyond a self-signature, so this bounds the heap an
/// arbitrary peer can make the gateway hold.
const MAX_REGISTRATIONS: usize = 100_000;

/// How long a registration stays wakeable without being refreshed. Devices
/// re-register on foreground, well inside this.
const REGISTRATION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Entries reclaimed in one pass once capacity is hit, so the sort is
/// amortised over many registrations rather than run per insert.
const EVICT_BATCH: usize = MAX_REGISTRATIONS / 10;

/// A stored device wake target under a pseudonym `P`.
#[derive(Debug, Clone)]
pub struct TokenEntry {
    pub provider: PushProvider,
    pub token:    Vec<u8>,
    refreshed_at: Instant,
}

impl TokenEntry {
    fn is_live_at(&self, now: Instant) -> bool {
        now.duration_since(self.refreshed_at) < REGISTRATION_TTL
    }
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
    /// attribute to the holder of `P`. At [`MAX_REGISTRATIONS`] the least
    /// recently refreshed entries are dropped.
    pub fn register(&self, reg: &RegisterToken) -> Result<(), &'static str> {
        if !reg.verify() {
            return Err("bad registration signature");
        }
        let now = Instant::now();
        let mut map = self.map.write();
        if map.len() >= MAX_REGISTRATIONS && !map.contains_key(&reg.pseudonym.0) {
            map.retain(|_, e| e.is_live_at(now));
            if map.len() >= MAX_REGISTRATIONS {
                evict_oldest(&mut map, EVICT_BATCH);
            }
        }
        map.insert(reg.pseudonym.0, TokenEntry {
            provider:     reg.provider,
            token:        reg.token.clone(),
            refreshed_at: now,
        });
        Ok(())
    }

    /// Look up a pseudonym's current wake target (for a `WakeRequest`).
    pub fn resolve(&self, pseudonym: &[u8; 32]) -> Option<TokenEntry> {
        let now = Instant::now();
        self.map.read().get(pseudonym).filter(|e| e.is_live_at(now)).cloned()
    }

    /// Drop registrations past [`REGISTRATION_TTL`]. Driven by the acceptor's
    /// maintenance loop.
    pub fn sweep(&self) {
        self.sweep_at(Instant::now());
    }

    fn sweep_at(&self, now: Instant) {
        let mut map = self.map.write();
        map.retain(|_, e| e.is_live_at(now));
        map.shrink_to_fit();
    }
}

fn evict_oldest(map: &mut HashMap<[u8; 32], TokenEntry>, count: usize) {
    let mut by_age: Vec<_> = map.iter().map(|(p, e)| (e.refreshed_at, *p)).collect();
    by_age.sort_unstable();
    for (_, p) in by_age.into_iter().take(count) {
        map.remove(&p);
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    fn entry(refreshed_at: Instant) -> TokenEntry {
        TokenEntry { provider: PushProvider::Fcm, token: b"tok".to_vec(), refreshed_at }
    }

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

    #[test]
    fn entry_expires_after_ttl() {
        let now = Instant::now();
        let e = entry(now);
        assert!(e.is_live_at(now + REGISTRATION_TTL - Duration::from_secs(1)));
        assert!(!e.is_live_at(now + REGISTRATION_TTL));
    }

    #[test]
    fn evicts_least_recently_refreshed_first() {
        let base = Instant::now();
        let mut map = HashMap::new();
        map.insert([1u8; 32], entry(base));
        map.insert([2u8; 32], entry(base + Duration::from_secs(1)));
        map.insert([3u8; 32], entry(base + Duration::from_secs(2)));

        evict_oldest(&mut map, 2);

        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&[3u8; 32]));
    }

    #[test]
    fn sweep_drops_expired_entries() {
        let base = Instant::now();
        let registry = PushRegistry::default();
        {
            let mut map = registry.map.write();
            map.insert([4u8; 32], entry(base));
            map.insert([5u8; 32], entry(base + REGISTRATION_TTL));
        }

        registry.sweep_at(base + REGISTRATION_TTL + Duration::from_secs(1));

        let map = registry.map.read();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&[5u8; 32]));
    }
}
