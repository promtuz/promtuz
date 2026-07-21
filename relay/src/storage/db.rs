//! The relay's on-disk store: one fjall `Database`, several keyspaces.
//!
//! Keyspaces (fjall's column-family equivalent — each its own LSM-tree):
//! - `messages`       sender-relay local fallback queue (`MessageKey` -> DispatchP).
//! - `dht_queue`      home-replica offline queue (`MessageKey`, per-recipient prefix).
//! - `dht_keypackage` MLS KeyPackage stash (per-IPK prefix).
//! - `dht_welcome`    MLS Welcome stash (per-recipient prefix).
//!
//! fjall does exact prefix scans natively, so no prefix-extractor config is
//! needed (unlike RocksDB). Durability-critical writes go through
//! [`Store::put_sync`] (insert + fsync); everything else is journal-buffered.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use fjall::Database;
use fjall::Keyspace;
use fjall::KeyspaceCreateOptions;
use fjall::PersistMode;
use fjall::UserKey;
use fjall::UserValue;

pub const KS_MESSAGES: &str = "messages";
pub const KS_DHT_QUEUE: &str = "dht_queue";
pub const KS_DHT_KEYPACKAGE: &str = "dht_keypackage";
pub const KS_DHT_WELCOME: &str = "dht_welcome";
pub const KS_LAST_SEEN: &str = "last_seen";
pub const KS_PRESENCE_CONSENT: &str = "presence_consent";
pub const KS_PRESENCE_STATE: &str = "presence_state";
pub const KS_PRESENCE_LEASE: &str = "presence_lease";
pub const KS_DHT_PUSH_PSEUDONYM: &str = "dht_push_pseudonym";
pub const KS_DHT_PUSH_PENDING: &str = "dht_push_pending";

/// Owns the relay's fjall `Database` and its keyspace handles. Shared as
/// `Arc<Store>` between the `Relay` (message queue) and the `Dht` (home
/// queue, MLS stashes) — both point at the same on-disk store.
pub struct Store {
    db: Database,
    pub messages: Keyspace,
    pub queue: Keyspace,
    pub keypackage: Keyspace,
    pub welcome: Keyspace,
    /// IPK (32B) -> last-disconnect unix-ms (u64 BE). Powers presence last-seen.
    pub last_seen: Keyspace,
    /// `(owner, recipient)` -> newest signed consent or revocation tombstone.
    pub presence_consent: Keyspace,
    pub presence_state: Keyspace,
    pub presence_lease: Keyspace,
    pub push_pseudonym: Keyspace,
    pub push_pending: Keyspace,
}

impl std::fmt::Debug for Store {
    // fjall's `Database` / `Keyspace` handles aren't `Debug`; `Dht` and
    // `Relay` derive `Debug` and hold an `Arc<Store>`, so give them a stub.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open (creating if absent) the relay's fjall store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::builder(path).open().context("open fjall database")?;
        let messages =
            db.keyspace(KS_MESSAGES, KeyspaceCreateOptions::default).context("open `messages`")?;
        let queue = db
            .keyspace(KS_DHT_QUEUE, KeyspaceCreateOptions::default)
            .context("open `dht_queue`")?;
        let keypackage = db
            .keyspace(KS_DHT_KEYPACKAGE, KeyspaceCreateOptions::default)
            .context("open `dht_keypackage`")?;
        let welcome = db
            .keyspace(KS_DHT_WELCOME, KeyspaceCreateOptions::default)
            .context("open `dht_welcome`")?;
        let last_seen = db
            .keyspace(KS_LAST_SEEN, KeyspaceCreateOptions::default)
            .context("open `last_seen`")?;
        let presence_consent = db
            .keyspace(KS_PRESENCE_CONSENT, KeyspaceCreateOptions::default)
            .context("open `presence_consent`")?;
        let presence_state = db
            .keyspace(KS_PRESENCE_STATE, KeyspaceCreateOptions::default)
            .context("open `presence_state`")?;
        let presence_lease = db
            .keyspace(KS_PRESENCE_LEASE, KeyspaceCreateOptions::default)
            .context("open `presence_lease`")?;
        let push_pseudonym = db
            .keyspace(KS_DHT_PUSH_PSEUDONYM, KeyspaceCreateOptions::default)
            .context("open `dht_push_pseudonym`")?;
        let push_pending = db
            .keyspace(KS_DHT_PUSH_PENDING, KeyspaceCreateOptions::default)
            .context("open `dht_push_pending`")?;
        Ok(Self {
            db,
            messages,
            queue,
            keypackage,
            welcome,
            last_seen,
            presence_consent,
            presence_state,
            presence_lease,
            push_pseudonym,
            push_pending,
        })
    }

    /// Record when a peer was last foreground-active (unix-ms) — stamped only on
    /// leaving an Active state, never on a background connect/disconnect, so a
    /// wake doesn't read as "seen now". Buffered, not fsynced — a lost stamp on
    /// crash just degrades to "last-seen unknown".
    pub fn put_last_seen(&self, ipk: &[u8; 32], ts_ms: u64) -> fjall::Result<()> {
        self.last_seen.insert(ipk, ts_ms.to_be_bytes())
    }

    /// Read a peer's last-disconnect time, `None` if never recorded.
    pub fn get_last_seen(&self, ipk: &[u8; 32]) -> Option<u64> {
        let v = self.last_seen.get(ipk).ok().flatten()?;
        Some(u64::from_be_bytes(v.as_ref().try_into().ok()?))
    }

    pub fn put_presence_consent(
        &self, consent: &common::proto::dht_p2p::PresenceConsent,
    ) -> fjall::Result<bool> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(&consent.owner.0);
        key[32..].copy_from_slice(&consent.recipient.0);
        if self.presence_consent.get(key)?.is_some_and(|v| {
            v.get(..8)
                .and_then(|n| n.try_into().ok())
                .map(u64::from_be_bytes)
                .is_some_and(|old| old >= consent.version)
        }) {
            return Ok(false);
        }
        let mut value = Vec::with_capacity(17);
        value.extend_from_slice(&consent.version.to_be_bytes());
        value.extend_from_slice(&consent.issued_at_ms.to_be_bytes());
        value.push(consent.granted as u8);
        self.put_sync(&self.presence_consent, key, value)?;
        Ok(true)
    }

    pub fn has_presence_consent(&self, ipk: &[u8; 32], contact: &[u8; 32]) -> bool {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(ipk);
        key[32..].copy_from_slice(contact);
        self.presence_consent.get(key).ok().flatten().is_some_and(|v| v.get(16) == Some(&1))
    }

    pub fn put_presence_state(
        &self, recipient: &[u8; 32], contact: &[u8; 32],
        state: &common::proto::client_rel::PresenceState, version: u64, observed_at_ms: u64,
    ) -> fjall::Result<bool> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(recipient);
        key[32..].copy_from_slice(contact);
        if self.presence_state.get(key)?.is_some_and(|v| {
            let old_version = v.get(..8).and_then(|n| n.try_into().ok()).map(u64::from_be_bytes);
            let old_observed = v.get(8..16).and_then(|n| n.try_into().ok()).map(u64::from_be_bytes);
            old_version.is_some_and(|old| old >= version)
                || old_observed.is_some_and(|old| old >= observed_at_ms)
        }) {
            return Ok(false);
        }
        let (tag, timestamp) = match state {
            common::proto::client_rel::PresenceState::Online => (0, 0),
            common::proto::client_rel::PresenceState::Idle { since } => (1, *since),
            common::proto::client_rel::PresenceState::Offline { last_seen } => (2, *last_seen),
        };
        let mut value = Vec::with_capacity(25);
        value.extend_from_slice(&version.to_be_bytes());
        value.extend_from_slice(&observed_at_ms.to_be_bytes());
        value.push(tag);
        value.extend_from_slice(&timestamp.to_be_bytes());
        self.presence_state.insert(key, value)?;
        Ok(true)
    }
    pub fn get_presence_state(
        &self, recipient: &[u8; 32], contact: &[u8; 32],
    ) -> Option<common::proto::client_rel::PresenceState> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(recipient);
        key[32..].copy_from_slice(contact);
        let value = self.presence_state.get(key).ok().flatten()?;
        let value = value.as_ref();
        let timestamp = u64::from_be_bytes(value.get(17..25)?.try_into().ok()?);
        match *value.get(16)? {
            0 => Some(common::proto::client_rel::PresenceState::Online),
            1 => Some(common::proto::client_rel::PresenceState::Idle { since: timestamp }),
            2 => Some(common::proto::client_rel::PresenceState::Offline { last_seen: timestamp }),
            _ => None,
        }
    }

    pub fn put_presence_lease(
        &self, lease: &common::proto::dht_p2p::PresenceLease,
    ) -> fjall::Result<bool> {
        use common::proto::pack::Packer;
        use common::proto::pack::Unpacker;

        if self.presence_lease.get(&lease.user.0)?.is_some_and(|v| {
            common::proto::dht_p2p::PresenceLease::deser(&v)
                .ok()
                .is_some_and(|old| old.version >= lease.version)
        }) {
            return Ok(false);
        }
        let Ok(value) = lease.ser() else { return Ok(false) };
        self.put_sync(&self.presence_lease, &lease.user.0, value)?;
        Ok(true)
    }

    pub fn get_presence_lease(&self, user: &[u8; 32]) -> Option<common::proto::dht_p2p::PresenceLease> {
        use common::proto::pack::Unpacker;

        common::proto::dht_p2p::PresenceLease::deser(&self.presence_lease.get(user).ok().flatten()?).ok()
    }

    /// Durable home-side `IPK -> P` mapping. `P` is opaque to the relay and
    /// cannot reveal a platform token without the push gateway's database.
    pub fn put_push_pseudonym(&self, ipk: &[u8; 32], pseudonym: &[u8; 32]) -> fjall::Result<()> {
        self.put_sync(&self.push_pseudonym, ipk, pseudonym)
    }

    pub fn get_push_pseudonym(&self, ipk: &[u8; 32]) -> Option<[u8; 32]> {
        self.push_pseudonym.get(ipk).ok().flatten()?.as_ref().try_into().ok()
    }

    pub fn put_pending_push(
        &self, publish: &common::proto::dht_p2p::PushPseudonymPublish,
    ) -> fjall::Result<()> {
        use common::proto::pack::Packer;

        let Ok(value) = publish.ser() else { return Ok(()) };
        self.put_sync(&self.push_pending, &publish.user_ipk.0, value)
    }

    pub fn remove_pending_push(&self, ipk: &[u8; 32]) -> fjall::Result<()> {
        self.push_pending.remove(ipk)?;
        self.db.persist(fjall::PersistMode::SyncAll)
    }

    pub fn pending_pushes(&self) -> Vec<common::proto::dht_p2p::PushPseudonymPublish> {
        use common::proto::pack::Unpacker;

        self.push_pending
            .iter()
            .filter_map(|entry| entry.into_inner().ok().and_then(|(_, value)| {
                common::proto::dht_p2p::PushPseudonymPublish::deser(&value).ok()
            }))
            .collect()
    }

    /// Insert then fsync the journal — the durability contract the old
    /// `WriteOptions::set_sync(true)` writes relied on.
    pub fn put_sync(
        &self, ks: &Keyspace, key: impl Into<UserKey>, val: impl Into<UserValue>,
    ) -> fjall::Result<()> {
        ks.insert(key, val)?;
        self.db.persist(PersistMode::SyncAll)
    }

    /// A buffered, atomic multi-op batch (used for drain GC). Not fsynced — a
    /// crash re-delivers, and the client dedupes by id.
    pub fn batch(&self) -> fjall::OwnedWriteBatch {
        self.db.batch()
    }

    /// Delete every entry in all keyspaces and fsync. Live-safe: the relay owns
    /// the fjall writer, so no lock fight — the `pzrelay clear-db` reset path.
    /// Leaves the daemon's in-memory routing/connections intact.
    pub fn clear_all(&self) -> Result<usize> {
        let mut n = 0usize;
        for ks in [
            &self.messages,
            &self.queue,
            &self.keypackage,
            &self.welcome,
            &self.last_seen,
            &self.presence_consent,
            &self.presence_state,
            &self.presence_lease,
            &self.push_pseudonym,
            &self.push_pending,
        ] {
            let keys: Vec<UserKey> = ks
                .iter()
                .map(|g| g.into_inner().map(|(k, _)| k))
                .collect::<fjall::Result<_>>()
                .context("iterate keyspace")?;
            for k in keys {
                ks.remove(k).context("remove key")?;
                n += 1;
            }
        }
        self.db.persist(PersistMode::SyncAll).context("persist after clear")?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    fn fresh_store() -> Store {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("pz-cleardb-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Store::open(&path).expect("open store")
    }

    #[test]
    fn clear_all_empties_every_keyspace() {
        let store = fresh_store();
        store.messages.insert("a".as_bytes(), "1".as_bytes()).unwrap();
        store.queue.insert("b".as_bytes(), "2".as_bytes()).unwrap();
        store.keypackage.insert("c".as_bytes(), "3".as_bytes()).unwrap();
        store.welcome.insert("d".as_bytes(), "4".as_bytes()).unwrap();
        store.last_seen.insert("e".as_bytes(), "5".as_bytes()).unwrap();

        let n = store.clear_all().expect("clear");
        assert_eq!(n, 5, "must report every deleted entry");
        for ks in
            [&store.messages, &store.queue, &store.keypackage, &store.welcome, &store.last_seen]
        {
            assert_eq!(ks.iter().count(), 0, "keyspace must be empty after clear");
        }
    }

    #[test]
    fn last_seen_roundtrips_and_defaults_to_none() {
        let store = fresh_store();
        let ipk = [7u8; 32];
        assert_eq!(store.get_last_seen(&ipk), None, "unrecorded IPK is None");
        store.put_last_seen(&ipk, 1_700_000_000_000).unwrap();
        assert_eq!(store.get_last_seen(&ipk), Some(1_700_000_000_000));
    }
}
