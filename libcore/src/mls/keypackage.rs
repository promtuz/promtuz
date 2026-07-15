//! Client-side KeyPackage stash management.
//!
//! Mints, persists, and tracks the client's pool of one-time
//! KeyPackages (RFC 9420 §5).
//!
//! # Roles in the data model
//!
//! Each [`KeyPackageRecord`] (in `common::proto::mls_wire`) carries:
//!
//! - **`ipk`** — the user's long-term Ed25519 IPK (32 B). The
//!   `BasicCredential::identity` field of the inner `KeyPackage` is
//!   set to the same bytes; this is how a verifier ties a fetched KP
//!   back to a known IPK.
//! - **`kp_ref`** — RFC 9420 §5.2 `KeyPackageRef`. We obtain it via
//!   [`openmls::prelude::KeyPackage::hash_ref`], which evaluates
//!   `SHA-256("MLS 1.0 KeyPackage Reference" ‖ tls_encode(kp))` for
//!   our cipher suite (suite `0x0003` mandates SHA-256). We do **not**
//!   compute a separate BLAKE3 ref — the codebase otherwise prefers
//!   BLAKE3, but `kp_ref` is RFC-mandated SHA-256.
//! - **`kp_bytes`** — the TLS-encoded `KeyPackage` itself, opaque to
//!   promtuz; verifies as a KP only after a fetcher decodes it back
//!   through openmls.
//! - **`expires_at_ms`** — `not_after` from the KP's lifetime
//!   extension, expressed in ms (openmls's `Lifetime` is in seconds;
//!   we multiply).
//! - **`owner_sig`** — Ed25519 signature under the user's IPK over
//!   [`kp_record_signing_input`]; the home relay verifies before
//!   accepting the publish.
//!
//! # Persistence model
//!
//! Two storage layers cooperate:
//!
//! 1. **openmls's `StorageProvider`** — owns the leaf-key bundle
//!    (`KeyPackageBundle`: HPKE init private key + encryption private
//!    key) keyed by `kp_ref`. `KeyPackage::builder().build(...)` writes
//!    to it; on Welcome receipt, openmls reads it back to instantiate
//!    the joining MlsGroup. This is opaque to us — we just provide
//!    the [`PromtuzMlsProvider`].
//!
//! 2. **Our `mls_keypackage_stash` table** — promtuz-side bookkeeping
//!    (kp_ref → generation/expiry/consumed timestamps). Drives:
//!    - Refill scheduling (count of unconsumed in-lifetime KPs).
//!    - Anti-pinning rotation
//!      ([`KP_SCHEDULED_ROTATION_MS`] cadence).
//!    - Consumption signal ([`KeyPackageStash::on_consumed`] —
//!      hooked when a Welcome arrives).
//!
//! Splitting these two layers means the openmls storage can persist a
//! KP we've never tracked (e.g. test fixtures bypassing
//! `KeyPackageStash`), and our table can outlive a leaf-key purge
//! (so we know which `kp_ref`s were ours even after consumption nulls
//! out their bundle). Both are written transactionally enough for the
//! single-threaded libcore caller — no two-phase commit needed.
//!
//! # Trust boundary on the Signer
//!
//! [`KeyPackageStash::generate_one`] takes an
//! `&ed25519_dalek::SigningKey` (the IPK identity signer). We do
//! **not** call [`crate::data::identity::IdentitySigner`] here because
//! `IdentitySigner` is workspace-coupled to `KEY_MANAGER` for unit
//! tests; pushing the signer down to a concrete `SigningKey` lets
//! both production callers (which decrypt the IPK secret via
//! `IdentitySigner` and pass it in) and tests (which mint a fresh
//! signing key) use the same code path.
//!
//! A thin convenience wrapper that pulls the IPK secret through
//! `IdentitySigner` and forwards to this module is planned.
//!


use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use common::proto::mls_wire::KEYPACKAGE_LIFETIME_MS;
use common::proto::mls_wire::KP_SCHEDULED_ROTATION_MS;
use common::proto::mls_wire::KP_STASH_LOW_WATER;
use common::proto::mls_wire::KP_STASH_TARGET;
use common::proto::mls_wire::KeyPackageRecord;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::kp_record_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use openmls::prelude::BasicCredential;
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

// `PROMTUZ_CIPHERSUITE` is the single cipher suite used across promtuz.
// Defined once in `mls::group` and re-exported from `mls::mod`; we
// import it here to keep this module independent of the rest of `group.rs`.
use super::group::PROMTUZ_CIPHERSUITE;
use super::provider::PromtuzMlsProvider;
use super::types::PromtuzMlsStorageError;

/// Result alias for fallible stash operations.
pub type Result<T> = std::result::Result<T, KeyPackageStashError>;

/// Errors raised by [`KeyPackageStash`].
///
/// We distinguish openmls failures (KP build, hash_ref, sig verify) from
/// our own SQLite-side failures so callers can attribute precisely.
#[derive(Debug, Error)]
pub enum KeyPackageStashError {
    /// Underlying SQLite call failed (open, query, write).
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A SQLite write that should have been transactional couldn't
    /// commit. Wrapped separately because we surface partial-state
    /// via storage error to callers.
    #[error("storage: {0}")]
    Storage(#[from] PromtuzMlsStorageError),

    /// openmls's `KeyPackage::builder().build(...)` failed (rare —
    /// usually a storage backing error). Wrapped via `Debug` for the
    /// same reason `MlsGroupError::OpenMls` does.
    #[error("openmls KeyPackage build failed: {0}")]
    OpenMlsBuild(String),

    /// `tls_codec` (TLS-encoding the KP) failed.
    #[error("tls_codec: {0}")]
    Codec(String),

    /// Generating the leaf signing keypair failed.
    #[error("openmls leaf signing keypair build failed: {0}")]
    LeafKeyBuild(String),

    /// `kp_ref` could not be computed.
    #[error("openmls hash_ref failed: {0}")]
    HashRef(String),
}

/// Client-side KeyPackage stash.
///
/// Holds an `Arc<Mutex<Connection>>` over the libcore MLS SQLite DB.
/// Construction reads the `mls_keypackage_stash` table to seed the
/// in-memory unconsumed counter; subsequent mutations keep the
/// counter in sync with the table. Counter access is internally
/// synchronised via `parking_lot::Mutex` (the same primitive the rest
/// of libcore uses) — never held across `.await`.
///
/// The struct is `Clone`-cheap (Arc clones).
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

impl KeyPackageStash {
    /// Construct a stash over the libcore MLS DB shared by
    /// [`PromtuzMlsProvider`].
    ///
    /// The connection must already have the MLS migrations applied
    /// (which include `mls_keypackage_stash`); in production the
    /// `MLS_DB` singleton handles that, in tests use
    /// [`crate::db::mls::apply_mls_migrations`].
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    // -----------------------------------------------------------------
    // Generation
    // -----------------------------------------------------------------

    /// Produce a single fresh KeyPackage and persist it.
    ///
    /// Steps:
    /// 1. Build a `BasicCredential` carrying the IPK bytes.
    /// 2. Generate a fresh `SignatureKeyPair` (the leaf signing key —
    ///    distinct from IPK, see `signer.rs` doc-comment). Persist it
    ///    via `SignatureKeyPair::store(provider.storage())` so openmls
    ///    can find it on Welcome receipt.
    /// 3. Build the openmls `KeyPackage` with our pinned cipher suite,
    ///    a 30-day lifetime, and a `Capabilities` advertising only
    ///    suite `0x0003`. The build call writes the
    ///    `KeyPackageBundle` (KP + init+enc private keys) into
    ///    `provider.storage()` keyed by `kp_ref`.
    /// 4. Compute `kp_ref` via openmls's `KeyPackage::hash_ref` (RFC
    ///    9420 §5.2 = SHA-256 of label-prefixed TLS-encoded KP under
    ///    suite 0x0003).
    /// 5. TLS-serialise the KP into `kp_bytes`.
    /// 6. Sign `kp_record_signing_input(MLS_WIRE_VERSION, ipk, kp_ref,
    ///    expires_at_ms)` under the IPK to produce `owner_sig`.
    /// 7. Insert the bookkeeping row into `mls_keypackage_stash`.
    ///
    /// Returns the fully-formed [`KeyPackageRecord`]. The corresponding
    /// leaf-key bundle is now persisted in openmls's storage and will
    /// be retrieved automatically when a Welcome consuming this KP
    /// arrives.
    pub fn generate_one(
        &self, provider: &PromtuzMlsProvider, ipk_signer: &SigningKey,
    ) -> Result<KeyPackageRecord> {
        let now = now_ms();
        let ipk: [u8; 32] = ipk_signer.verifying_key().to_bytes();

        // 1. Credential.
        let credential = BasicCredential::new(ipk.to_vec());

        // 2. Leaf signing keypair. We use the basic-credential crate's
        // `SignatureKeyPair::new(SignatureScheme::ED25519)` so openmls's
        // `Signer::store` slot is wired up — that's how openmls retrieves
        // the secret half on Welcome receipt.
        let leaf_kp = SignatureKeyPair::new(SignatureScheme::ED25519)
            .map_err(|e| KeyPackageStashError::LeafKeyBuild(format!("{e:?}")))?;
        leaf_kp
            .store(provider.storage())
            .map_err(KeyPackageStashError::Storage)?;

        let cwk = CredentialWithKey {
            credential: credential.into(),
            signature_key: leaf_kp.public().into(),
        };

        // 3. Lifetime — the openmls `Lifetime::new(t)` adds an
        // automatic 1h "before now" margin (see openmls's lifetime.rs).
        // The `not_after` becomes `now_secs + lifetime_secs`. Spec
        // says 30 days = `KEYPACKAGE_LIFETIME_MS / 1000 = 2_592_000`.
        // `Lifetime::has_acceptable_range` allows up to ~3 months + 1h,
        // so 30 days is well within bounds.
        let lifetime_secs = KEYPACKAGE_LIFETIME_MS / 1000;
        let lifetime = Lifetime::new(lifetime_secs);
        let expires_at_ms = lifetime.not_after().saturating_mul(1000);

        // 3b. Build the KP. This writes the bundle into
        // openmls's storage keyed by hash_ref.
        let bundle = KeyPackage::builder()
            .key_package_lifetime(lifetime)
            .leaf_node_capabilities(Capabilities::new(
                None, /* protocol versions: openmls picks `Mls10` */
                Some(&[PROMTUZ_CIPHERSUITE]),
                None, /* extensions */
                None, /* proposals */
                None, /* credentials */
            ))
            .build(PROMTUZ_CIPHERSUITE, provider, &leaf_kp, cwk)
            .map_err(|e| KeyPackageStashError::OpenMlsBuild(format!("{e:?}")))?;

        let kp = bundle.key_package().clone();

        // 4. KeyPackageRef per RFC 9420 §5.2.
        let kp_ref = kp
            .hash_ref(provider.crypto())
            .map_err(|e| KeyPackageStashError::HashRef(format!("{e:?}")))?;
        let kp_ref_bytes: Vec<u8> = kp_ref.as_slice().to_vec();

        // 5. TLS-encode KP.
        let kp_bytes = kp
            .tls_serialize_detached()
            .map_err(|e| KeyPackageStashError::Codec(e.to_string()))?;

        // 6. Owner sig (under the IPK). The transcript binds
        // `BLAKE3(kp_bytes)` so a stolen IPK cannot mint bogus
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

        // 7. Persist bookkeeping row + the full serialized record so we
        // can republish this KP on reconnect without re-minting.
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

    /// Unconsumed, in-lifetime records with a stored `record_blob`,
    /// ready to (re)publish to a relay. Legacy rows minted before the
    /// `record_blob` column are skipped (they carry no republishable
    /// record); such clients re-mint a full stash on their next connect.
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

    /// Drop unconsumed records whose `owner_sig` no longer verifies under the
    /// CURRENT [`MLS_WIRE_VERSION`] — the transcript mixes the version in, so
    /// a wire bump silently invalidates every previously-minted record. Left
    /// in place they poison the stash twice over: they count toward the
    /// stash target (suppressing re-mints) AND get republished verbatim on
    /// every reconnect, so every peer's fetch fails with "owner_sig invalid".
    /// The sig check IS the version check — no schema change needed. Returns
    /// the number purged; `ensure_stash_full` then mints replacements and the
    /// full-snapshot Publish evicts the stale copies relay-side.
    pub fn purge_invalid_records(&self, now_ms: u64) -> usize {
        use ed25519_dalek::Signature;
        use ed25519_dalek::VerifyingKey;

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
                    let Ok(vk) = VerifyingKey::from_bytes(&rec.ipk.0) else {
                        return Some(kp_ref);
                    };
                    let msg = kp_record_signing_input(
                        MLS_WIRE_VERSION,
                        &rec.ipk.0,
                        &rec.kp_ref.0,
                        &rec.kp_bytes.0,
                        rec.expires_at_ms,
                    );
                    let sig = Signature::from_bytes(&rec.owner_sig.0);
                    vk.verify_strict(&msg, &sig).is_err().then_some(kp_ref)
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

    /// Fill the stash up to [`KP_STASH_TARGET`] unconsumed in-lifetime
    /// KPs. Returns the freshly-generated records (so the caller can
    /// publish them via the QUIC client's `DhtClient::publish_keypackages`).
    ///
    /// Idempotent: if the stash is already at target, returns an empty
    /// vec without minting anything.
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

    // -----------------------------------------------------------------
    // Bookkeeping
    // -----------------------------------------------------------------

    /// Mark a `kp_ref` as consumed. The Welcome-receipt path calls
    /// this when a Welcome arrives that consumed one of our stashed
    /// KPs (the recipient's libcore looks up the leaf-key
    /// bundle by `kp_ref` and then ratchets the bookkeeping table to
    /// drop the unconsumed counter).
    ///
    /// Idempotent: a second call on the same `kp_ref` is a no-op.
    /// A `kp_ref` we don't recognize is also a no-op (the row may
    /// have been pruned during a rotation).
    pub fn on_consumed(&self, kp_ref: &[u8]) -> Result<()> {
        let conn = self.db.lock();
        conn.execute(
            "UPDATE mls_keypackage_stash SET consumed = 1 WHERE kp_ref = ?1",
            params![kp_ref],
        )?;
        Ok(())
    }

    /// True iff the unconsumed-in-lifetime count has dropped below
    /// [`KP_STASH_LOW_WATER`].
    ///
    /// Called by the background scheduler to decide whether a refill
    /// round is due. A `now_ms` parameter rather than reading the wall
    /// clock so tests can pin a deterministic value.
    pub fn should_refill(&self, now_ms: u64) -> bool {
        self.count_unconsumed_in_lifetime(now_ms) < KP_STASH_LOW_WATER
    }

    /// True iff a periodic anti-pinning rotation is due — the oldest
    /// surviving unconsumed KP was generated more than
    /// [`KP_SCHEDULED_ROTATION_MS`] ago.
    ///
    /// Even with no consumption, we rotate the stash periodically so a
    /// malicious peer that hoarded fetches can use them only within the
    /// rotation window. After rotation, the old (in-lifetime) KPs remain
    /// consumable until natural expiry — what changes is that *new*
    /// fetches return fresher KPs.
    ///
    /// Returns `false` on an empty stash (nothing to rotate).
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
                age_ms >= KP_SCHEDULED_ROTATION_MS
            }
            _ => false,
        }
    }

    /// Periodic anti-pinning rotation hook.
    ///
    /// If [`Self::should_rotate`] returns `true`, mints
    /// [`KP_STASH_TARGET`] fresh KeyPackages. The freshly-minted
    /// records are returned so the caller can publish them. Old KPs
    /// remain in the stash until natural expiry — we don't evict them
    /// proactively because they're still valid for already-in-flight
    /// adds.
    ///
    /// Returns `Ok(Vec::new())` when no rotation is due.
    pub fn rotate_periodic(
        &self, provider: &PromtuzMlsProvider, ipk_signer: &SigningKey, now_ms: u64,
    ) -> Result<Vec<KeyPackageRecord>> {
        if !self.should_rotate(now_ms) {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(KP_STASH_TARGET);
        for _ in 0..KP_STASH_TARGET {
            out.push(self.generate_one(provider, ipk_signer)?);
        }
        Ok(out)
    }

    /// Count of unconsumed records whose `expires_at_ms > now_ms`.
    ///
    /// Public for use by the scheduler to surface a UI-level "your
    /// stash is healthy" indicator. Bounded query (`COUNT(*)`), no full
    /// table scan.
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wall-clock now in ms. Same idiom as the rest of libcore.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ed25519_dalek::Signature;
    use ed25519_dalek::Verifier;
    use ed25519_dalek::VerifyingKey;
    use sha2::Digest;
    use sha2::Sha256;

    use super::*;
    use crate::db::mls::apply_mls_migrations;

    /// Open a fresh in-memory MLS DB and wrap it in the shared
    /// connection handle the stash + provider both consume.
    fn fresh_conn() -> Arc<Mutex<Connection>> {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_mls_migrations(&mut conn);
        Arc::new(Mutex::new(conn))
    }

    /// Build a stash + provider that share the same SQLite connection
    /// — production code uses the `MLS_DB` singleton, but tests need
    /// an in-memory connection that both halves see.
    fn build_pair() -> (KeyPackageStash, PromtuzMlsProvider) {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn);
        (stash, provider)
    }

    fn fresh_ipk_signer() -> SigningKey {
        // Deterministic-seeded for reproducibility.
        SigningKey::from_bytes(&[0x42u8; 32])
    }

    /// A record signed under the PREVIOUS wire version (what a pre-bump build
    /// leaves behind) must be purged; a current-version record must survive.
    /// Without the purge, stale records suppress re-minting and get
    /// republished — every peer's pairing then fails with "owner_sig invalid".
    #[test]
    fn purge_invalid_records_drops_stale_version_signatures() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = fresh_ipk_signer();

        let good = stash.generate_one(&provider, &signer).expect("gen good");
        let mut stale = stash.generate_one(&provider, &signer).expect("gen stale");
        let old_msg = kp_record_signing_input(
            MLS_WIRE_VERSION - 1,
            &stale.ipk.0,
            &stale.kp_ref.0,
            &stale.kp_bytes.0,
            stale.expires_at_ms,
        );
        stale.owner_sig = signer.sign(&old_msg).to_bytes().into();
        let blob = stale.ser().expect("ser");
        conn.lock()
            .execute(
                "UPDATE mls_keypackage_stash SET record_blob = ?1 WHERE kp_ref = ?2",
                params![&blob, stale.kp_ref.0.as_slice()],
            )
            .expect("swap in stale blob");

        assert_eq!(stash.purge_invalid_records(now_ms()), 1, "stale record purged");
        let left = stash.unconsumed_records(now_ms()).expect("read");
        assert_eq!(left.len(), 1, "exactly the valid record remains");
        assert_eq!(left[0].kp_ref.0, good.kp_ref.0, "current-version record survives");
    }

    // -----------------------------------------------------------------
    // 1. Single KP generation produces a valid record
    // -----------------------------------------------------------------

    #[test]
    fn generate_one_produces_signed_record() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();
        let rec = stash.generate_one(&provider, &signer).expect("gen");

        let ipk: [u8; 32] = signer.verifying_key().to_bytes();
        assert_eq!(rec.ipk.0, ipk);
        assert_eq!(rec.kp_ref.0.len(), 32, "kp_ref is 32 bytes");
        // RFC 9420 §5.2: kp_ref is a label-prefixed HashReference, *not*
        // the naive SHA-256(kp_bytes) (which would be a spec deviation).
        let naive = Sha256::digest(rec.kp_bytes.0.as_slice());
        assert_ne!(
            rec.kp_ref.0.as_slice(),
            naive.as_slice(),
            "kp_ref must use RFC 9420 §5.2 HashReference (with label \
             prefix), not raw SHA-256(kp_bytes)"
        );
        assert!(!rec.kp_bytes.0.is_empty(), "kp_bytes non-empty");
        assert!(rec.expires_at_ms > now_ms(), "lifetime in the future");

        // Owner sig verifies under the IPK.
        let vk = VerifyingKey::from_bytes(&rec.ipk.0).expect("vk");
        let sig = Signature::from_bytes(&rec.owner_sig.0);
        let msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &rec.ipk.0,
            &rec.kp_ref.0,
            &rec.kp_bytes.0,
            rec.expires_at_ms,
        );
        vk.verify(&msg, &sig).expect("owner sig verifies");
    }

    // -----------------------------------------------------------------
    // 2. ensure_stash_full mints KP_STASH_TARGET records on a fresh DB
    // -----------------------------------------------------------------

    #[test]
    fn ensure_stash_full_mints_target_records() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();

        let recs = stash.ensure_stash_full(&provider, &signer).expect("fill");
        assert_eq!(recs.len(), KP_STASH_TARGET);
        // Each record must have its own kp_ref.
        let unique: HashSet<Vec<u8>> = recs.iter().map(|r| r.kp_ref.0.clone()).collect();
        assert_eq!(unique.len(), KP_STASH_TARGET, "no kp_ref collisions");

        // Counter agrees.
        assert_eq!(
            stash.count_unconsumed_in_lifetime(now_ms()),
            KP_STASH_TARGET
        );

        // Second call is a no-op (idempotent).
        let recs2 = stash.ensure_stash_full(&provider, &signer).expect("fill again");
        assert!(recs2.is_empty());
    }

    // -----------------------------------------------------------------
    // 4. Owner sig verifies — covered by test 1; here we additionally
    // pin that *changing* any signed field invalidates the sig.
    // -----------------------------------------------------------------

    #[test]
    fn owner_sig_is_change_sensitive() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();
        let rec = stash.generate_one(&provider, &signer).expect("gen");

        let vk = VerifyingKey::from_bytes(&rec.ipk.0).expect("vk");
        let sig = Signature::from_bytes(&rec.owner_sig.0);

        // Tampered kp_ref → verify fails.
        let mut tampered_ref = rec.kp_ref.0.clone();
        tampered_ref[0] ^= 0xFF;
        let bad_msg = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &rec.ipk.0,
            &tampered_ref,
            &rec.kp_bytes.0,
            rec.expires_at_ms,
        );
        assert!(vk.verify(&bad_msg, &sig).is_err());

        // Tampered expires_at_ms → verify fails.
        let bad_msg2 = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &rec.ipk.0,
            &rec.kp_ref.0,
            &rec.kp_bytes.0,
            rec.expires_at_ms.wrapping_add(1),
        );
        assert!(vk.verify(&bad_msg2, &sig).is_err());

        // Tampered kp_bytes → verify fails (the `BLAKE3(kp_bytes)`
        // binding prevents kp_bytes substitution).
        let mut tampered_bytes = rec.kp_bytes.0.clone();
        tampered_bytes[0] ^= 0xFF;
        let bad_msg3 = kp_record_signing_input(
            MLS_WIRE_VERSION,
            &rec.ipk.0,
            &rec.kp_ref.0,
            &tampered_bytes,
            rec.expires_at_ms,
        );
        assert!(
            vk.verify(&bad_msg3, &sig).is_err(),
            "tampered kp_bytes must invalidate the owner sig"
        );
    }

    // -----------------------------------------------------------------
    // 5. Persistence — re-opening a stash sees the same count
    // -----------------------------------------------------------------

    #[test]
    fn stash_count_survives_restart() {
        // Both halves of the test must share the *same* SQLite handle
        // (an in-memory DB only persists per-connection). So we
        // simulate "restart" by dropping the old `KeyPackageStash`
        // value and constructing a fresh one over the same Arc<Mutex>.
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let signer = fresh_ipk_signer();

        // Step 1: mint 5 KPs.
        let stash_a = KeyPackageStash::new(conn.clone());
        for _ in 0..5 {
            stash_a.generate_one(&provider, &signer).expect("gen");
        }
        assert_eq!(stash_a.count_unconsumed_in_lifetime(now_ms()), 5);
        drop(stash_a);

        // Step 2: open a fresh stash over the same DB; counter is
        // recovered from the table.
        let stash_b = KeyPackageStash::new(conn);
        assert_eq!(stash_b.count_unconsumed_in_lifetime(now_ms()), 5);
    }

    // -----------------------------------------------------------------
    // 6. Anti-pinning rotation triggers when threshold hit
    // -----------------------------------------------------------------

    #[test]
    fn rotation_triggers_when_oldest_kp_exceeds_cadence() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = fresh_ipk_signer();

        // Mint a single KP at "now = 100" to fix the generated_at_ms
        // baseline. We can't force `generate_one` to use a specific
        // wall-clock, but we *can* manipulate the stash table directly
        // for the test (the production path is what we test in test 1
        // and test 2; this test is about the rotation predicate).
        stash.generate_one(&provider, &signer).expect("gen");

        // Reach into the SQLite directly to age the KP. Tests own the
        // DB; this is a deterministic-fixture pattern.
        {
            let conn = conn.lock();
            conn.execute(
                "UPDATE mls_keypackage_stash SET generated_at_ms = ?1",
                params![100i64],
            )
            .expect("age");
        }

        // At "now = 100 + KP_SCHEDULED_ROTATION_MS / 2", no rotation
        // due.
        let mid = 100 + (KP_SCHEDULED_ROTATION_MS / 2);
        assert!(!stash.should_rotate(mid));

        // At "now = 100 + KP_SCHEDULED_ROTATION_MS", rotation due
        // (>= boundary). Spec uses the cadence as a soft trigger so
        // boundary-eligible is acceptable.
        let due = 100 + KP_SCHEDULED_ROTATION_MS;
        assert!(stash.should_rotate(due));
    }

    #[test]
    fn rotate_periodic_mints_full_batch_when_due() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = fresh_ipk_signer();

        // Mint one KP and age it to be rotation-eligible.
        stash.generate_one(&provider, &signer).expect("gen");

        // Freshly-minted KP → not yet due; rotate_periodic is a no-op.
        let recs_noop = stash
            .rotate_periodic(&provider, &signer, now_ms())
            .expect("rotate");
        assert!(recs_noop.is_empty());

        {
            let conn = conn.lock();
            conn.execute(
                "UPDATE mls_keypackage_stash SET generated_at_ms = ?1",
                params![100i64],
            )
            .expect("age");
        }

        let due = 100 + KP_SCHEDULED_ROTATION_MS;
        let recs = stash
            .rotate_periodic(&provider, &signer, due)
            .expect("rotate");
        assert_eq!(recs.len(), KP_STASH_TARGET);
    }

    // -----------------------------------------------------------------
    // 8. should_refill below low water
    // -----------------------------------------------------------------

    #[test]
    fn should_refill_when_count_below_low_water() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();

        // Empty stash → should_refill = true.
        assert!(stash.should_refill(now_ms()));

        // Mint KP_STASH_LOW_WATER - 1 KPs → still under threshold.
        for _ in 0..(KP_STASH_LOW_WATER - 1) {
            stash.generate_one(&provider, &signer).expect("gen");
        }
        assert!(stash.should_refill(now_ms()));

        // Mint one more → at threshold; predicate is `< low_water` so
        // this is *not* refill-due.
        stash.generate_one(&provider, &signer).expect("gen");
        assert!(!stash.should_refill(now_ms()));
    }

    // -----------------------------------------------------------------
    // 9. on_consumed flips the bookkeeping flag
    // -----------------------------------------------------------------

    #[test]
    fn on_consumed_decrements_unconsumed_count() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();
        let rec = stash.generate_one(&provider, &signer).expect("gen");
        assert_eq!(stash.count_unconsumed_in_lifetime(now_ms()), 1);

        stash.on_consumed(&rec.kp_ref.0).expect("consume");
        assert_eq!(stash.count_unconsumed_in_lifetime(now_ms()), 0);

        // Idempotent.
        stash.on_consumed(&rec.kp_ref.0).expect("consume again");
        assert_eq!(stash.count_unconsumed_in_lifetime(now_ms()), 0);
    }

    // -----------------------------------------------------------------
    // 10. Lifetime boundary: an expired KP doesn't count toward
    // unconsumed-in-lifetime.
    // -----------------------------------------------------------------

    #[test]
    fn count_excludes_expired_kps() {
        let conn = fresh_conn();
        let provider = PromtuzMlsProvider::new(conn.clone());
        let stash = KeyPackageStash::new(conn.clone());
        let signer = fresh_ipk_signer();

        let rec = stash.generate_one(&provider, &signer).expect("gen");
        assert_eq!(stash.count_unconsumed_in_lifetime(now_ms()), 1);

        // Move the wall clock past expiry.
        let past_expiry = rec.expires_at_ms + 1;
        assert_eq!(stash.count_unconsumed_in_lifetime(past_expiry), 0);
    }

    // -----------------------------------------------------------------
    // 11. unconsumed_records returns the persisted, republishable records
    // -----------------------------------------------------------------

    #[test]
    fn unconsumed_records_returns_persisted_records() {
        let (stash, provider) = build_pair();
        let signer = fresh_ipk_signer();

        let minted: Vec<_> = (0..3)
            .map(|_| stash.generate_one(&provider, &signer).expect("gen"))
            .collect();

        let recs = stash.unconsumed_records(now_ms()).expect("records");
        assert_eq!(recs.len(), 3);

        // Every minted kp_ref is present and each record round-trips.
        let got: HashSet<Vec<u8>> = recs.iter().map(|r| r.kp_ref.0.clone()).collect();
        for m in &minted {
            assert!(got.contains(&m.kp_ref.0), "minted kp_ref missing");
        }
        for r in &recs {
            let round = KeyPackageRecord::deser(&r.ser().expect("ser")).expect("deser");
            assert_eq!(&round, r, "record must round-trip via ser/deser");
        }

        // Consumed records drop out.
        stash.on_consumed(&minted[0].kp_ref.0).expect("consume");
        assert_eq!(stash.unconsumed_records(now_ms()).expect("records").len(), 2);
    }
}
