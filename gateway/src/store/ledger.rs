//! Pack ownership, published versions and blob references. Published blobs
//! remain available to old messages; only abandoned uploads may be removed.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

pub struct Ledger(Connection);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub creator: [u8; 32],
    pub version: u32,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let db =
            Connection::open(path).with_context(|| format!("opening ledger {}", path.display()))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS packs (
                 pack       BLOB PRIMARY KEY,
                 creator    BLOB NOT NULL,
                 version    INTEGER NOT NULL,
                 claimed_at INTEGER NOT NULL
             ) WITHOUT ROWID;
             CREATE INDEX IF NOT EXISTS packs_by_creator ON packs (creator);
             CREATE TABLE IF NOT EXISTS blobs (
                 pack   BLOB NOT NULL,
                 key    BLOB NOT NULL,
                 named  INTEGER NOT NULL,
                 put_at INTEGER NOT NULL,
                 PRIMARY KEY (pack, key)
             ) WITHOUT ROWID;",
        )
        .context("ledger schema")?;
        Ok(Self(db))
    }

    fn count(&self, sql: &str, p: impl rusqlite::Params) -> Result<usize> {
        let n: i64 = self.0.query_row(sql, p, |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn pack_count(&self) -> Result<usize> {
        self.count("SELECT count(*) FROM packs", [])
    }

    pub fn get(&self, pack: &[u8; 16]) -> Result<Option<PackEntry>> {
        Ok(self
            .0
            .query_row("SELECT creator, version FROM packs WHERE pack = ?1", [pack], |r| {
                Ok(PackEntry { creator: r.get(0)?, version: r.get(1)? })
            })
            .optional()?)
    }

    pub fn packs_of(&self, creator: &[u8; 32]) -> Result<usize> {
        self.count("SELECT count(*) FROM packs WHERE creator = ?1", [creator])
    }

    #[cfg(test)]
    pub fn blob_count(&self, pack: &[u8; 16]) -> Result<usize> {
        self.count("SELECT count(*) FROM blobs WHERE pack = ?1", [pack])
    }

    pub fn named_keys(&self, pack: &[u8; 16]) -> Result<Vec<[u8; 32]>> {
        Ok(self
            .0
            .prepare("SELECT key FROM blobs WHERE pack = ?1 AND named = 1")?
            .query_map([pack], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Blobs no manifest has named yet.
    pub fn unnamed_count(&self, pack: &[u8; 16]) -> Result<usize> {
        self.count("SELECT count(*) FROM blobs WHERE pack = ?1 AND named = 0", [pack])
    }

    pub fn has_blob(&self, pack: &[u8; 16], key: &[u8; 32]) -> Result<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT 1 FROM blobs WHERE pack = ?1 AND key = ?2",
                params![pack, key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Bind a pack to its creator at `version`; a known pack is left as is.
    pub fn claim(&self, pack: &[u8; 16], creator: &[u8; 32], version: u32, now: u64) -> Result<()> {
        self.0.execute(
            "INSERT OR IGNORE INTO packs (pack, creator, version, claimed_at) VALUES (?1, ?2, ?3, ?4)",
            params![pack, creator, version, now],
        )?;
        Ok(())
    }

    /// A blob the bucket now holds; claims the pack if this is its first object.
    pub fn add_blob(
        &mut self, pack: &[u8; 16], creator: &[u8; 32], key: &[u8; 32], now: u64,
    ) -> Result<()> {
        let tx = self.0.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO packs (pack, creator, version, claimed_at) VALUES (?1, ?2, 0, ?3)",
            params![pack, creator, now],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO blobs (pack, key, named, put_at) VALUES (?1, ?2, 0, ?3)",
            params![pack, key, now],
        )?;
        Ok(tx.commit()?)
    }

    /// Record a stored manifest and return unreferenced blobs for deletion.
    /// Call [`forget`](Self::forget) only after each object is deleted.
    pub fn publish(
        &mut self, pack: &[u8; 16], creator: &[u8; 32], version: u32, keys: &[[u8; 32]], now: u64,
    ) -> Result<Vec<[u8; 32]>> {
        let tx = self.0.transaction()?;
        tx.execute(
            "INSERT INTO packs (pack, creator, version, claimed_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (pack) DO UPDATE SET version = excluded.version",
            params![pack, creator, version, now],
        )?;
        for key in keys {
            // The store also verifies objects missing from a recovered ledger.
            tx.execute(
                "INSERT INTO blobs (pack, key, named, put_at) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT (pack, key) DO UPDATE SET named = 1",
                params![pack, key, now],
            )?;
        }
        let unnamed = tx
            .prepare("SELECT key FROM blobs WHERE pack = ?1 AND named = 0")?
            .query_map([pack], |r| r.get(0))?
            .collect::<Result<Vec<[u8; 32]>, _>>()?;
        tx.commit()?;
        Ok(unnamed)
    }

    pub fn forget(&self, pack: &[u8; 16], key: &[u8; 32]) -> Result<()> {
        self.0.execute("DELETE FROM blobs WHERE pack = ?1 AND key = ?2", params![pack, key])?;
        Ok(())
    }

    /// Remove an unpublished pack once all its blobs have been deleted.
    pub fn drop_pack(&self, pack: &[u8; 16]) -> Result<()> {
        self.0.execute("DELETE FROM packs WHERE pack = ?1 AND version = 0 AND NOT EXISTS (SELECT 1 FROM blobs WHERE pack = ?1)", [pack])?;
        Ok(())
    }

    /// Packs that never got a manifest, and blobs no manifest named, from
    /// before `before`.
    pub fn expired(&self, before: u64) -> Result<(Vec<[u8; 16]>, Vec<([u8; 16], [u8; 32])>)> {
        let packs = self
            .0
            .prepare("SELECT pack FROM packs WHERE version = 0 AND claimed_at < ?1")?
            .query_map([before], |r| r.get(0))?
            .collect::<Result<Vec<[u8; 16]>, _>>()?;
        let blobs = self
            .0
            .prepare("SELECT pack, key FROM blobs WHERE named = 0 AND put_at < ?1")?
            .query_map([before], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((packs, blobs))
    }
}
