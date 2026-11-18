//! The KeyPackage stash. openmls storage holds each KeyPackage's private keys;
//! `mls_keypackage_stash` tracks the records we minted, their expiry and consumption.

use std::sync::Arc;

use common::crypto::verify_ed25519;
use common::proto::mls_wire::KEYPACKAGE_LIFETIME_MS;
use common::proto::mls_wire::KP_SCHEDULED_ROTATION_MS;

/// 2026-08-23T00:00Z: when KeyPackages started binding their leaf to the
/// IPK. A stash with anything older rotates at once.
const BOUND_CREDENTIALS_SINCE_MS: u64 = 1_787_443_200_000;
use common::proto::mls_wire::KP_STASH_LOW_WATER;
use common::proto::mls_wire::KP_STASH_TARGET;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::kp_record_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::utils::now_ms;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use openmls::prelude::Capabilities;
use openmls::prelude::CredentialWithKey;
use openmls::prelude::KeyPackage;
use openmls::prelude::Lifetime;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::OpenMlsProvider;
use openmls_traits::types::SignatureScheme;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::params;
use thiserror::Error;

use super::group::GROUP_META_EXTENSION;
use super::group::PROMTUZ_CIPHERSUITE;
use super::provider::PromtuzMlsProvider;
use super::types::PromtuzMlsStorageError;

pub type Result<T> = std::result::Result<T, KeyPackageStashError>;

#[derive(Debug, Error)]
pub enum KeyPackageStashError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("storage: {0}")]
    Storage(#[from] PromtuzMlsStorageError),

    #[error("openmls KeyPackage build failed: {0}")]
    OpenMlsBuild(String),

    #[error("tls_codec: {0}")]
    Codec(String),

    #[error("openmls leaf signing keypair build failed: {0}")]
    LeafKeyBuild(String),

    #[error("openmls hash_ref failed: {0}")]
    HashRef(String),
}

#[derive(Clone)]
pub struct KeyPackageStash {
    db: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for KeyPackageStash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyPackageStash")
            .field("unconsumed_count", &self.count_unconsumed_in_lifetime(now_ms()))
            .finish_non_exhaustive()
    }
}

/// A leaf without the group-metadata extension cannot join a group chat, so its KeyPackage must be
/// re-minted. An undecodable KeyPackage counts as lacking it.
fn declares_group_meta(kp_bytes: &[u8]) -> bool {
    use openmls::prelude::KeyPackageIn;
    use openmls::prelude::ProtocolVersion;
    use openmls::prelude::tls_codec::Deserialize as _;

    let Ok(kp_in) = KeyPackageIn::tls_deserialize_exact(kp_bytes) else { return false };
    let crypto = openmls_rust_crypto::RustCrypto::default();
    let Ok(kp) = kp_in.validate(&crypto, ProtocolVersion::Mls10) else { return false };
    kp.leaf_node()
        .capabilities()
        .extensions()
        .contains(&GROUP_META_EXTENSION)
}

impl KeyPackageStash {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub fn generate_one(
        &self, provider: &PromtuzMlsProvider, ipk_signer: &SigningKey,
    ) -> Result<KeyPackageRecord> {
        let now = now_ms();
        let ipk: [u8; 32] = ipk_signer.verifying_key().to_bytes();

        // Stored so `leaf_signer_for_group` can read it back once a Welcome seats this leaf.
        let leaf_kp = SignatureKeyPair::new(SignatureScheme::ED25519)
            .map_err(|e| KeyPackageStashError::LeafKeyBuild(format!("{e:?}")))?;
        leaf_kp
            .store(provider.storage())
            .map_err(KeyPackageStashError::Storage)?;

        let credential = super::credential::bound_credential(ipk_signer, leaf_kp.public());
        let cwk = CredentialWithKey {
            credential: credential.into(),
            signature_key: leaf_kp.public().into(),
        };

        // `Lifetime::new` backdates `not_before` by an hour; `not_after` is now plus the lifetime.
        let lifetime_secs = KEYPACKAGE_LIFETIME_MS / 1000;
        let lifetime = Lifetime::new(lifetime_secs);
        let expires_at_ms = lifetime.not_after().saturating_mul(1000);

        // Building also stores the private bundle in openmls storage, keyed by hash_ref.
        let bundle = KeyPackage::builder()
            .key_package_lifetime(lifetime)
            .leaf_node_capabilities(Capabilities::new(
                None, /* protocol versions: openmls picks `Mls10` */
                Some(&[PROMTUZ_CIPHERSUITE]),
                // Groups carry their metadata in a context extension, and RFC 9420 refuses to add
                // a leaf that does not declare every extension in the context.
                Some(&[GROUP_META_EXTENSION]),
                None, /* proposals */
                None, /* credentials */
            ))
            .build(PROMTUZ_CIPHERSUITE, provider, &leaf_kp, cwk)
            .map_err(|e| KeyPackageStashError::OpenMlsBuild(format!("{e:?}")))?;

        let kp = bundle.key_package().clone();

        let kp_ref = kp
            .hash_ref(provider.crypto())
            .map_err(|e| KeyPackageStashError::HashRef(format!("{e:?}")))?;
        let kp_ref_bytes: Vec<u8> = kp_ref.as_slice().to_vec();

        let kp_bytes = kp
            .tls_serialize_detached()
            .map_err(|e| KeyPackageStashError::Codec(e.to_string()))?;

        // The owner sig binds `BLAKE3(kp_bytes)`, so a stolen IPK cannot mint bogus
        // `(ipk, kp_ref, fake_kp_bytes)` triples.
        let signing_input = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &ipk,
            &kp_ref_bytes,
            &kp_bytes,
            expires_at_ms,
        );
        let owner_sig = ipk_signer.sign(&signing_input);

        let record = KeyPackageRecord {
            ipk: ipk.into(),
            kp_ref: kp_ref_bytes.clone().into(),
            kp_bytes: kp_bytes.into(),
            expires_at_ms,
            owner_sig: owner_sig.to_bytes().into(),
        };
        let record_blob = record
            .ser()
            .map_err(|e| KeyPackageStashError::Codec(format!("ser kp record: {e}")))?;

        {
            let conn = self.db.lock();
            conn.execute(
                "INSERT OR REPLACE INTO mls_keypackage_stash \
                    (kp_ref, generated_at_ms, expires_at_ms, consumed, record_blob) \
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![&kp_ref_bytes, now as i64, expires_at_ms as i64, &record_blob],
            )?;
        }

        Ok(record)
    }

    pub fn unconsumed_records(&self, now_ms: u64) -> Result<Vec<KeyPackageRecord>> {
        let conn = self.db.lock();
        // Cap at KP_STASH_TARGET: the home rejects a larger batch (TooMany),
        // and rotation can leave >target unconsumed locally.
        let mut stmt = conn.prepare(
            "SELECT record_blob FROM mls_keypackage_stash \
             WHERE consumed = 0 AND expires_at_ms > ?1 AND record_blob IS NOT NULL \
             ORDER BY generated_at_ms DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now_ms as i64, KP_STASH_TARGET as i64], |r| {
            r.get::<_, Vec<u8>>(0)
        })?;
        let mut out = Vec::new();
        for blob in rows {
            out.push(
                KeyPackageRecord::deser(&blob?)
                    .map_err(|e| KeyPackageStashError::Codec(format!("deser kp record: {e}")))?,
            );
        }
        Ok(out)
    }

    /// Deletes unconsumed records peers cannot use: an `owner_sig` that fails under the current
    /// [`MLS_WIRE_VERSION`], or a leaf without the group-metadata extension. Left in place they
    /// count toward the target and get republished.
    pub fn purge_invalid_records(&self, now_ms: u64) -> usize {
        let conn = self.db.lock();
        let stale: Vec<Vec<u8>> = {
            let Ok(mut stmt) = conn.prepare(
                "SELECT kp_ref, record_blob FROM mls_keypackage_stash \
                 WHERE consumed = 0 AND expires_at_ms > ?1 AND record_blob IS NOT NULL",
            ) else {
                return 0;
            };
            let Ok(rows) = stmt.query_map(params![now_ms as i64], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            }) else {
                return 0;
            };
            rows.flatten()
                .filter_map(|(kp_ref, blob)| {
                    let Ok(rec) = KeyPackageRecord::deser(&blob) else {
                        return Some(kp_ref); // undecodable blob = stale too
                    };
                    let msg = kp_record_signing_input(
                        MLS_WIRE_VERSION,
                        &rec.ipk.0,
                        &rec.kp_ref.0,
                        &rec.kp_bytes.0,
                        rec.expires_at_ms,
                    );
                    if verify_ed25519(&rec.ipk.0, &msg, &rec.owner_sig.0).is_err() {
                        return Some(kp_ref);
                    }
                    (!declares_group_meta(&rec.kp_bytes.0)).then_some(kp_ref)
                })
                .collect()
        };
        let mut n = 0usize;
        for kp_ref in &stale {
            n += conn
                .execute("DELETE FROM mls_keypackage_stash WHERE kp_ref = ?1", [kp_ref])
                .unwrap_or(0);
        }
        n
    }

    pub fn ensure_stash_full(
        &self, provider: &PromtuzMlsProvider, ipk_signer: &SigningKey,
    ) -> Result<Vec<KeyPackageRecord>> {
        let now = now_ms();
        let existing = self.count_unconsumed_in_lifetime(now);
        let to_mint = KP_STASH_TARGET.saturating_sub(existing);
        let mut out = Vec::with_capacity(to_mint);
        for _ in 0..to_mint {
            out.push(self.generate_one(provider, ipk_signer)?);
        }
        Ok(out)
    }

    pub fn on_consumed(&self, kp_ref: &[u8]) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE mls_keypackage_stash SET consumed = 1 WHERE kp_ref = ?1",
            params![kp_ref],
        )?;
        Ok(())
    }

    pub fn should_refill(&self, now_ms: u64) -> bool {
        self.count_unconsumed_in_lifetime(now_ms) < KP_STASH_LOW_WATER
    }

    pub fn should_rotate(&self, now_ms: u64) -> bool {
        let conn = self.db.lock();
        let oldest_gen: Option<i64> = conn
            .query_row(
                "SELECT MIN(generated_at_ms) FROM mls_keypackage_stash \
                 WHERE consumed = 0 AND expires_at_ms > ?1",
                params![now_ms as i64],
                |r| r.get(0),
            )
            .unwrap_or(None);
        match oldest_gen {
            Some(g) if g >= 0 => {
                let age_ms = now_ms.saturating_sub(g as u64);
                // Anything minted before the credential binding existed
                // carries a bare IPK, which no group chat will seat.
                age_ms >= KP_SCHEDULED_ROTATION_MS
                    || (now_ms >= BOUND_CREDENTIALS_SINCE_MS && (g as u64) < BOUND_CREDENTIALS_SINCE_MS)
            }
            _ => false,
        }
    }

    /// Mints a new generation and retires the old one (`consumed = 2`), so only the new one is
    /// published. A retired record stays usable until it expires: its Welcome may be on its way.
    pub fn rotate_periodic(
        &self, provider: &PromtuzMlsProvider, ipk_signer: &SigningKey, now_ms: u64,
    ) -> Result<Vec<KeyPackageRecord>> {
        if !self.should_rotate(now_ms) {
            return Ok(Vec::new());
        }
        // Snapshot before minting, so the new rows are not retired with the old.
        let superseded: Vec<Vec<u8>> = {
            let conn = self.db.lock();
            let mut stmt =
                conn.prepare("SELECT kp_ref FROM mls_keypackage_stash WHERE consumed = 0")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            rows.flatten().collect()
        };

        let mut out = Vec::with_capacity(KP_STASH_TARGET);
        for _ in 0..KP_STASH_TARGET {
            out.push(self.generate_one(provider, ipk_signer)?);
        }

        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            {
                let mut stmt =
                    tx.prepare("UPDATE mls_keypackage_stash SET consumed = 2 WHERE kp_ref = ?1")?;
                for kp_ref in &superseded {
                    stmt.execute([kp_ref])?;
                }
            }
            tx.commit()?;
        }
        self.sweep_expired(provider, now_ms)?;
        Ok(out)
    }

    /// Deletes expired records that never joined a group, with the KeyPackage bundle and leaf
    /// signing key openmls holds for them.
    pub fn sweep_expired(&self, provider: &PromtuzMlsProvider, now_ms: u64) -> Result<()> {
        use openmls::prelude::KeyPackageIn;
        use openmls::prelude::KeyPackageRef;
        use openmls::prelude::tls_codec::Deserialize as _;
        use openmls_traits::storage::StorageProvider as _;

        let expired: Vec<(Vec<u8>, Option<Vec<u8>>)> = {
            let conn = self.db.lock();
            let mut stmt = conn.prepare(
                "SELECT kp_ref, record_blob FROM mls_keypackage_stash \
                 WHERE consumed != 1 AND expires_at_ms <= ?1",
            )?;
            let rows = stmt.query_map(params![now_ms as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.flatten().collect()
        };
        for (kp_ref, blob) in &expired {
            if let Some(rec) = blob.as_deref().and_then(|b| KeyPackageRecord::deser(b).ok()) {
                // RFC 9420 5.2: the reference is the labelled hash of the encoded package.
                let reference = KeyPackageRef::new(
                    &rec.kp_bytes.0,
                    PROMTUZ_CIPHERSUITE,
                    provider.crypto(),
                    b"MLS 1.0 KeyPackage Reference",
                )
                .map_err(|e| KeyPackageStashError::HashRef(format!("{e:?}")))?;
                provider.storage().delete_key_package(&reference)?;
                if let Ok(kp) = KeyPackageIn::tls_deserialize_exact(&rec.kp_bytes.0) {
                    let key = kp.unverified_credential().signature_key;
                    SignatureKeyPair::delete(provider.storage(), key.as_slice(), SignatureScheme::ED25519)?;
                }
            }
            self.db.lock().execute("DELETE FROM mls_keypackage_stash WHERE kp_ref = ?1", [kp_ref])?;
        }
        Ok(())
    }

    pub fn count_unconsumed_in_lifetime(&self, now_ms: u64) -> usize {
        let conn = self.db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM mls_keypackage_stash \
             WHERE consumed = 0 AND expires_at_ms > ?1",
            params![now_ms as i64],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mls::Party;

    /// After a wire-version bump, records signed under the old version are purged rather than
    /// republished: peers reject their owner signature, so pairing with them fails.
    #[test]
    fn purge_invalid_records_drops_stale_version_signatures() {
        let party = Party::new(0x42);
        let stash = KeyPackageStash::new(party.db.clone());
        let good = stash.generate_one(&party.provider, &party.identity).unwrap();
        let mut stale = stash.generate_one(&party.provider, &party.identity).unwrap();
        let old = kp_record_signing_input(
            MLS_WIRE_VERSION - 1,
            &stale.ipk.0,
            &stale.kp_ref.0,
            &stale.kp_bytes.0,
            stale.expires_at_ms,
        );
        stale.owner_sig = party.identity.sign(&old).to_bytes().into();
        let sql = "UPDATE mls_keypackage_stash SET record_blob = ?1 WHERE kp_ref = ?2";
        party.db.lock().execute(sql, params![stale.ser().unwrap(), stale.kp_ref.0]).unwrap();

        assert_eq!(stash.purge_invalid_records(now_ms()), 1);
        let left = stash.unconsumed_records(now_ms()).unwrap();
        assert_eq!(left.into_iter().map(|r| r.kp_ref).collect::<Vec<_>>(), [good.kp_ref]);
    }
}
