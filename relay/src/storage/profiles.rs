//! One bounded record per owner/field, with atomic ciphertext and audience replacement.
use anyhow::{Result, ensure};
use common::crypto::verify_ed25519;
use common::proto::pack::{Packer, Unpacker};
use common::proto::profile::{
    Field, MAX_CIPHERTEXT, MAX_READERS, Publication, ReaderKey, Response,
};
use fjall::{Database, Keyspace, KeyspaceCreateOptions};

const MAX_BYTES: u64 = 1_000_000_000;
const MAX_IDENTITIES: usize = 100_000;

pub struct Profiles {
    db: Database,
    // ponytail: each read decodes at most MAX_READERS grants. Split indexed grants from the
    // payload if high-fanout profile reads become a measured bottleneck.
    values: Keyspace,
    keys: Keyspace,
    usage: Keyspace,
    // ponytail: publication is infrequent; one lock makes global admission exact.
    // Shard with a reserved-byte counter if measured write contention warrants it.
    write: parking_lot::Mutex<()>,
}

impl Profiles {
    pub fn open(db: &Database) -> Result<Self> {
        Ok(Self {
            db: db.clone(),
            values: db.keyspace("profile_values", KeyspaceCreateOptions::default)?,
            keys: db.keyspace("profile_reader_keys", KeyspaceCreateOptions::default)?,
            usage: db.keyspace("profile_usage", KeyspaceCreateOptions::default)?,
            write: parking_lot::Mutex::new(()),
        })
    }
    pub fn reader_key(&self, owner: &[u8; 32]) -> Result<Option<ReaderKey>> {
        self.keys.get(owner)?.map(|v| ReaderKey::deser(&v).map_err(Into::into)).transpose()
    }
    pub fn register(&self, owner: &[u8; 32], key: &ReaderKey) -> Result<bool> {
        ensure!(key.owner.0 == *owner, "profile reader owner mismatch");
        verify_ed25519(owner, &key.input(), &key.signature.0)?;
        let _guard = self.write.lock();
        if let Some(old) = self.reader_key(owner)? {
            // v1 derives this key from the identity secret; replacement requires a new protocol.
            return Ok(old == *key);
        }
        let count = self
            .usage
            .get(b"identities")?
            .map(|v| v.as_ref().try_into().map(u64::from_be_bytes))
            .transpose()?
            .unwrap_or(0);
        if count >= MAX_IDENTITIES as u64 {
            return Ok(false);
        }
        let mut batch = self.db.batch();
        batch.insert(&self.keys, owner, key.ser()?);
        batch.insert(&self.usage, b"identities", (count + 1).to_be_bytes());
        batch.commit()?;
        Ok(true)
    }
    pub fn publish(&self, owner: &[u8; 32], publication: &Publication) -> Result<bool> {
        let value = &publication.value;
        ensure!(
            value.owner.0 == *owner && value.version > 0 && value.version <= i64::MAX as u64,
            "invalid profile owner/version"
        );
        ensure!(value.ciphertext.len() <= MAX_CIPHERTEXT, "profile too large");
        ensure!(publication.grants.len() <= MAX_READERS, "too many profile readers");
        ensure!(
            publication.grants.windows(2).all(|g| g[0].reader.0 < g[1].reader.0),
            "readers must be unique and sorted"
        );
        verify_ed25519(owner, &value.input(), &value.signature.0)?;
        let _guard = self.write.lock();
        if self.reader_key(owner)?.is_none() {
            return Ok(false);
        }
        let key = [owner.as_slice(), &[value.field.id()]].concat();
        let bytes = publication.ser()?;
        let old = self.values.get(&key)?;
        if let Some(old) = &old {
            let previous = Publication::deser(old)?;
            if value.version <= previous.value.version {
                return Ok(bytes.as_slice() == old.as_ref());
            }
            // The logical object survives updates; no implicit migration/recreation on restore.
            if value.object != previous.value.object {
                return Ok(false);
            }
        }
        let used = self
            .usage
            .get(b"bytes")?
            .map(|v| v.as_ref().try_into().map(u64::from_be_bytes))
            .transpose()?
            .unwrap_or(0);
        let next = used
            .checked_sub(old.as_ref().map_or(0, |v| v.len()) as u64)
            .and_then(|n| n.checked_add(bytes.len() as u64))
            .ok_or_else(|| anyhow::anyhow!("profile usage overflow"))?;
        // Shrinking and empty-audience withdrawals remain possible under storage pressure.
        if next > used
            && (next > MAX_BYTES
                || self
                    .db
                    .disk_space()?
                    .saturating_add(self.db.write_buffer_size())
                    .saturating_add(bytes.len() as u64)
                    > super::queue::MAX_QUEUE_DB_BYTES)
        {
            return Ok(false);
        }
        let mut batch = self.db.batch();
        batch.insert(&self.values, key, bytes);
        batch.insert(&self.usage, b"bytes", next.to_be_bytes());
        batch.commit()?;
        Ok(true)
    }
    pub fn fetch(
        &self, reader: &[u8; 32], owner: &[u8; 32], field: Field, known: Option<u64>,
    ) -> Result<Response> {
        let key = [owner.as_slice(), &[field.id()]].concat();
        let Some(bytes) = self.values.get(key)? else { return Ok(Response::Missing) };
        let record = Publication::deser(&bytes)?;
        // Audience changes require a new version too; an unchanged version cannot revoke or
        // regrant anyone. This also avoids retransmitting ciphertext after a withdrawal.
        if known == Some(record.value.version) {
            return Ok(Response::Unchanged);
        }
        let Some(grant) = record.grants.iter().find(|g| g.reader.0 == *reader) else {
            return Ok(Response::Withdrawn { value: record.value });
        };
        Ok(Response::Value { value: record.value, grant: grant.clone() })
    }
    pub fn head(&self, owner: &[u8; 32], field: Field) -> Result<Response> {
        let key = [owner.as_slice(), &[field.id()]].concat();
        let record = self.values.get(key)?.map(|v| Publication::deser(&v)).transpose()?;
        Ok(Response::Head(record.map(|p| (p.value.object, p.value.version))))
    }
    pub fn readers(&self, owner: &[u8; 32], field: Field) -> Result<Vec<[u8; 32]>> {
        let key = [owner.as_slice(), &[field.id()]].concat();
        let record = self.values.get(key)?.map(|v| Publication::deser(&v)).transpose()?;
        Ok(record.map_or_else(Vec::new, |p| p.grants.into_iter().map(|g| g.reader.0).collect()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use common::proto::profile::{Grant, Value};
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn fixture(version: u64, readers: Vec<[u8; 32]>) -> (ReaderKey, Publication) {
        let key = SigningKey::from_bytes(&[31; 32]);
        let owner = key.verifying_key().to_bytes();
        let mut reader =
            ReaderKey { owner: owner.into(), key: [32; 32].into(), signature: [0; 64].into() };
        reader.signature = key.sign(&reader.input()).to_bytes().into();
        let mut value = Value {
            owner: owner.into(),
            field: Field::Avatar,
            object: [33; 32].into(),
            version,
            ciphertext: vec![version as u8; 40].into(),
            signature: [0; 64].into(),
        };
        value.signature = key.sign(&value.input()).to_bytes().into();
        let mut grants: Vec<_> = readers
            .into_iter()
            .map(|reader| Grant {
                reader: reader.into(),
                encapsulated: [34; 32].into(),
                wrapped_key: [35; 48].into(),
            })
            .collect();
        grants.sort_by_key(|g| g.reader.0);
        (reader, Publication { value, grants })
    }

    #[test]
    fn replacement_revocation_and_usage_survive_abrupt_exit() {
        const CHILD: &str = "PROMTUZ_PROFILE_CRASH_PATH";
        let (key, original) = fixture(1, vec![[42; 32]]);
        let owner = key.owner.0;
        let (_, revoked) = fixture(3, vec![]);
        if let Some(path) = std::env::var_os(CHILD) {
            let store = crate::storage::db::Store::open(path).unwrap();
            assert!(store.profiles.register(&owner, &key).unwrap());
            assert!(store.profiles.publish(&owner, &original).unwrap());
            assert!(store.profiles.publish(&owner, &original).unwrap(), "lost ACK retry");
            let (_, middle) = fixture(2, vec![[42; 32]]);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    store.profiles.publish(&owner, &middle).unwrap();
                });
                scope.spawn(|| {
                    store.profiles.publish(&owner, &revoked).unwrap();
                });
            });
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(store.persist_barrier().wait())
                .unwrap();
            std::process::exit(0);
        }
        let dir = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::profiles::tests::replacement_revocation_and_usage_survive_abrupt_exit",
            ])
            .env(CHILD, dir.path())
            .status()
            .unwrap();
        assert!(result.success());
        let store = crate::storage::db::Store::open(dir.path()).unwrap();
        assert_eq!(
            store.profiles.fetch(&[42; 32], &owner, Field::Avatar, Some(1)).unwrap(),
            Response::Withdrawn { value: revoked.value.clone() }
        );
        assert!(!store.profiles.publish(&owner, &original).unwrap());
        assert!(store.profiles.publish(&owner, &revoked).unwrap());
        assert_eq!(store.profiles.values.len().unwrap(), 1);
        let usage = store.profiles.usage.get(b"bytes").unwrap().unwrap();
        assert_eq!(
            u64::from_be_bytes(usage.as_ref().try_into().unwrap()),
            revoked.ser().unwrap().len() as u64
        );
        assert!(store.profiles.publish(&[9; 32], &original).is_err());
        let (_, mut invalid) = fixture(4, vec![[42; 32]]);
        invalid.value.ciphertext[0] ^= 1;
        assert!(store.profiles.publish(&owner, &invalid).is_err());
        let (_, mut invalid) = fixture(4, vec![[42; 32], [42; 32]]);
        assert!(store.profiles.publish(&owner, &invalid).is_err());
        invalid.grants.clear();
        invalid.value.ciphertext = vec![0; MAX_CIPHERTEXT + 1].into();
        assert!(store.profiles.publish(&owner, &invalid).is_err());
        assert_eq!(
            store.profiles.head(&owner, Field::Avatar).unwrap(),
            Response::Head(Some((revoked.value.object, 3)))
        );
    }
}
