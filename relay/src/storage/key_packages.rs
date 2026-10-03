//! Atomic, bounded KeyPackage custody. Network handlers verify signatures;
//! this owner serializes publish/pop and remembers consumption until expiry.
use common::proto::{
    mls_wire::{KP_STASH_TARGET, KeyPackageRecord, key_package_stash_prefix},
    pack::{Packer, Unpacker},
};
use common::types::id::NodeId;
use fjall::{Database, Keyspace, KeyspaceCreateOptions};
use parking_lot::Mutex;
use std::collections::BTreeMap;

pub const RECORDS: &str = "dht_keypackage";
pub const SPENT: &str = "dht_keypackage_spent_v1";
type Key = [u8; 64];
type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("key package static fields conflict")]
    Conflict,
    #[error("key package stash is full")]
    Full,
    #[error("invalid key package storage representation")]
    Invalid,
    #[error("key package storage failed: {0}")]
    Storage(#[from] fjall::Error),
}
pub struct KeyPackages {
    db: Database,
    pub(super) records: Keyspace,
    pub(super) spent: Keyspace,
    // Fixed stripes bound lock memory while unrelated owners can proceed.
    locks: [Mutex<()>; 64],
}
impl KeyPackages {
    pub(super) fn open(db: &Database) -> Result<Self> {
        Ok(Self {
            db: db.clone(),
            records: db.keyspace(RECORDS, KeyspaceCreateOptions::default)?,
            spent: db.keyspace(SPENT, KeyspaceCreateOptions::default)?,
            locks: std::array::from_fn(|_| Mutex::new(())),
        })
    }
    fn key(ipk: &[u8; 32], reference: &[u8]) -> Result<Key> {
        let mut key = [0u8; 64];
        key[..32].copy_from_slice(&key_package_stash_prefix(ipk));
        key[32..].copy_from_slice(<&[u8; 32]>::try_from(reference).map_err(|_| Error::Invalid)?);
        Ok(key)
    }
    fn entries(&self, ipk: &[u8; 32]) -> Result<BTreeMap<Key, KeyPackageRecord>> {
        let mut records = BTreeMap::new();
        for item in self.records.prefix(key_package_stash_prefix(ipk)) {
            let (key, value) = item.into_inner()?;
            let key: Key = key.as_ref().try_into().map_err(|_| Error::Invalid)?;
            let record = KeyPackageRecord::deser(&value).map_err(|_| Error::Invalid)?;
            if record.ipk.0 != *ipk || Self::key(ipk, &record.kp_ref.0)? != key {
                return Err(Error::Invalid);
            }
            records.insert(key, record);
            if records.len() > KP_STASH_TARGET {
                return Err(Error::Full);
            }
        }
        Ok(records)
    }
    /// Replaces the stash; any error leaves it unchanged. The caller must await the store's persist
    /// barrier before acking.
    pub fn publish(&self, ipk: &[u8; 32], incoming: &[KeyPackageRecord]) -> Result<()> {
        if incoming.len() > KP_STASH_TARGET {
            return Err(Error::Full);
        }
        let _lock = self.locks[usize::from(ipk[0]) % self.locks.len()].lock();
        let existing = self.entries(ipk)?;
        let mut final_records = BTreeMap::new();
        for record in incoming {
            if record.ipk.0 != *ipk {
                return Err(Error::Invalid);
            }
            let key = Self::key(ipk, &record.kp_ref.0)?;
            let bytes = record.ser().map_err(|_| Error::Invalid)?;
            if let Some(spent) = self.spent.get(key)? {
                if spent.len() != 40 {
                    return Err(Error::Invalid);
                }
                if spent[8..] != *NodeId::new(&bytes).as_bytes() {
                    return Err(Error::Conflict);
                }
                // An ambiguous publish retry cannot resurrect a consumed key.
                continue;
            }
            if existing.get(&key).is_some_and(|old| old != record)
                || final_records.get(&key).is_some_and(|old| old != record)
            {
                return Err(Error::Conflict);
            }
            final_records.insert(key, record.clone());
        }
        let mut batch = self.db.batch();
        for key in existing.keys().filter(|key| !final_records.contains_key(*key)) {
            batch.remove(&self.records, *key);
        }
        for (key, record) in final_records {
            batch.insert(&self.records, key, record.ser().map_err(|_| Error::Invalid)?);
        }
        batch.commit()?;
        Ok(())
    }
    /// Pops exactly once per replica: the spent marker and the removal commit together. Other
    /// replicas can still hand out the same package, so clients recover from collisions.
    pub fn take(
        &self, ipk: &[u8; 32], selector: &[u8; 32], now: u64,
    ) -> Result<Option<(KeyPackageRecord, u32)>> {
        let _lock = self.locks[usize::from(ipk[0]) % self.locks.len()].lock();
        let entries = self.entries(ipk)?;
        let mut batch = self.db.batch();
        let mut available = Vec::new();
        for (key, record) in entries {
            if record.expires_at_ms <= now {
                batch.remove(&self.records, key);
            } else {
                available.push((key, record));
            }
        }
        let chosen = available
            .iter()
            .enumerate()
            .min_by_key(|(_, (key, _))| {
                let mut distance = [0u8; 32];
                for i in 0..32 {
                    distance[i] = key[32 + i] ^ selector[i];
                }
                distance
            })
            .map(|(index, _)| index);
        let result = if let Some(chosen) = chosen {
            let remaining = available.len() - 1;
            let (key, record) = available.swap_remove(chosen);
            let mut marker = Vec::with_capacity(40);
            marker.extend_from_slice(&record.expires_at_ms.to_be_bytes());
            marker.extend_from_slice(
                NodeId::new(record.ser().map_err(|_| Error::Invalid)?).as_bytes(),
            );
            batch.insert(&self.spent, key, marker);
            batch.remove(&self.records, key);
            Some((record, remaining as u32))
        } else {
            None
        };
        batch.commit()?;
        Ok(result)
    }
    /// Complete, read-only view under the same lock as publish and consume.
    /// No fetch quota or spent marker changes, including for expired records.
    pub fn inventory(&self, ipk: &[u8; 32], now: u64) -> Result<Vec<[u8; 32]>> {
        let _lock = self.locks[usize::from(ipk[0]) % self.locks.len()].lock();
        self.entries(ipk)?.into_values().filter(|record| record.expires_at_ms > now)
            .map(|record| record.kp_ref.0.try_into().map_err(|_| Error::Invalid)).collect()
    }
    #[allow(dead_code)]
    pub fn count(&self) -> Result<usize> {
        Ok(self.records.len()?)
    }
}

pub(super) fn spent_expired(_key: &[u8], value: &[u8], now: u64) -> bool {
    value
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .is_some_and(|expires| expires <= now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Store;

    fn record(id: u8) -> KeyPackageRecord {
        KeyPackageRecord {
            ipk:           [7; 32].into(),
            kp_ref:        vec![id; 32].into(),
            kp_bytes:      vec![id; 128].into(),
            expires_at_ms: u64::MAX / 2,
            owner_sig:     [3; 64].into(),
        }
    }

    /// One-shot custody: under sixteen concurrent takes and across a restart, a consumed
    /// package is never vended again, even when its owner publishes it again.
    #[tokio::test]
    async fn a_consumed_package_is_never_vended_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let package = record(1);
        store.key_packages.publish(&[7; 32], &[package.clone()]).unwrap();
        let vended = std::thread::scope(|scope| {
            let takes: Vec<_> = (0..16)
                .map(|_| {
                    scope
                        .spawn(|| store.key_packages.take(&[7; 32], &[0; 32], 1).unwrap().is_some())
                })
                .collect();
            takes.into_iter().map(|take| take.join().unwrap()).filter(|vended| *vended).count()
        });
        assert_eq!(vended, 1);
        assert_eq!(
            (store.key_packages.count().unwrap(), store.key_packages.spent.len().unwrap()),
            (0, 1)
        );
        store.persist_barrier().wait().await.unwrap();
        drop(store);

        let store = Store::open(dir.path()).unwrap();
        store.key_packages.publish(&[7; 32], &[package]).unwrap();
        assert!(store.key_packages.take(&[7; 32], &[0; 32], 1).unwrap().is_none());
        store.key_packages.publish(&[7; 32], &[record(2)]).unwrap();
        assert_eq!(store.key_packages.take(&[7; 32], &[0; 32], 1).unwrap().unwrap().0, record(2));
    }

    /// A rejected snapshot leaves the whole previous stash, and an unreadable record is an
    /// error, never a stash silently consumed or reported empty.
    #[test]
    fn a_rejected_or_unreadable_stash_is_left_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let old = vec![record(1), record(2)];
        store.key_packages.publish(&[7; 32], &old).unwrap();
        let mut conflict = record(2);
        conflict.kp_bytes.0.push(1);
        let rejected = store.key_packages.publish(&[7; 32], &[record(3), conflict]);
        assert!(matches!(rejected, Err(Error::Conflict)));
        let mut kept: Vec<_> = (0..2)
            .map(|_| store.key_packages.take(&[7; 32], &[0; 32], 1).unwrap().unwrap().0)
            .collect();
        kept.sort_by(|a, b| a.kp_ref.0.cmp(&b.kp_ref.0));
        assert_eq!(kept, old);

        store.key_packages.publish(&[7; 32], &[record(4)]).unwrap();
        store
            .key_packages
            .records
            .insert(KeyPackages::key(&[7; 32], &[0; 32]).unwrap(), b"invalid")
            .unwrap();
        assert!(matches!(store.key_packages.take(&[7; 32], &[0; 32], 1), Err(Error::Invalid)));
        assert!(matches!(store.key_packages.inventory(&[7; 32], 1), Err(Error::Invalid)));
        assert_eq!(store.key_packages.count().unwrap(), 2, "nothing was consumed");
    }
}
