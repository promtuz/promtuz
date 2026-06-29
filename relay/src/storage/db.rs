//! The relay's on-disk store: one fjall `Database`, several keyspaces.
//!
//! Keyspaces (fjall's column-family equivalent — each its own LSM-tree):
//! - `messages`       sender-relay local fallback queue (`MessageKey` -> DispatchP).
//! - `dht_presence`   replica presence records + tombstones (point lookups).
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
pub const KS_DHT_PRESENCE: &str = "dht_presence";
pub const KS_DHT_QUEUE: &str = "dht_queue";
pub const KS_DHT_KEYPACKAGE: &str = "dht_keypackage";
pub const KS_DHT_WELCOME: &str = "dht_welcome";

/// Owns the relay's fjall `Database` and its keyspace handles. Shared as
/// `Arc<Store>` between the `Relay` (message queue) and the `Dht` (presence,
/// home queue, MLS stashes) — both point at the same on-disk store.
pub struct Store {
    db:             Database,
    pub messages:   Keyspace,
    pub presence:   Keyspace,
    pub queue:      Keyspace,
    pub keypackage: Keyspace,
    pub welcome:    Keyspace,
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
        let presence = db
            .keyspace(KS_DHT_PRESENCE, KeyspaceCreateOptions::default)
            .context("open `dht_presence`")?;
        let queue =
            db.keyspace(KS_DHT_QUEUE, KeyspaceCreateOptions::default).context("open `dht_queue`")?;
        let keypackage = db
            .keyspace(KS_DHT_KEYPACKAGE, KeyspaceCreateOptions::default)
            .context("open `dht_keypackage`")?;
        let welcome = db
            .keyspace(KS_DHT_WELCOME, KeyspaceCreateOptions::default)
            .context("open `dht_welcome`")?;
        Ok(Self { db, messages, presence, queue, keypackage, welcome })
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
}
