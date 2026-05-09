//! `PromtuzStorageProvider`: a rusqlite-backed implementation of
//! `openmls_traits::storage::StorageProvider<{CURRENT_VERSION}>`.
//!
//! Layout (see `misc/specs/MLS.md` §9.3 and `db::mls`):
//!
//! - One table `mls_storage(group_id, key_tag, sub_key, value)`.
//! - `group_id` is the CBOR-encoded openmls `GroupId`. For entries that
//!   aren't scoped to a single group (signature key pairs, key packages,
//!   PSKs, encryption key pairs), `group_id` is the empty BLOB.
//! - `key_tag` is a 1-byte discriminator from [`tags`].
//! - `sub_key` is the CBOR-encoded secondary key (a proposal ref, an
//!   epoch + leaf index, a public-key blob, …) — empty when the
//!   `(group_id, key_tag)` pair already addresses one row.
//! - `value` is the CBOR-encoded entity. List-shaped slots
//!   (`OwnLeafNodes`, `ProposalQueueRefs`) hold a `Vec<Vec<u8>>` of
//!   inner CBOR-encoded entries.
//!
//! Per-`group_id` writes are gated by [`super::MLS_GROUP_STATE_BUDGET_BYTES`].
//! On overflow the write is rejected with
//! [`PromtuzMlsStorageError::BudgetExceeded`] and *not* applied (we
//! never silently overwrite).
//!
//! All SQL uses parameter binding — adversarial `group_id` bytes
//! (quotes, NUL, …) round-trip unchanged.

use std::sync::Arc;

use openmls_traits::storage::traits;
use openmls_traits::storage::StorageProvider;
use openmls_traits::storage::CURRENT_VERSION;
use parking_lot::Mutex;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::types::PromtuzMlsStorageError;
use super::MLS_GROUP_STATE_BUDGET_BYTES;

/// Numeric discriminators for openmls `Entity` variants. Stored as
/// `key_tag` in `mls_storage`. Stable on-disk — never renumber.
///
/// All variants are reachable only through `StorageProvider` trait
/// dispatch (which the cdylib compiler can't see); silence the
/// dead-code lint module-wide rather than per-constant.
#[allow(dead_code)]
pub(crate) mod tags {
    pub const JOIN_CONFIG: i64 = 0x01;
    pub const OWN_LEAF_NODES: i64 = 0x02;
    pub const QUEUED_PROPOSAL: i64 = 0x03;
    pub const PROPOSAL_QUEUE_REFS: i64 = 0x04;
    pub const TREE: i64 = 0x05;
    pub const INTERIM_TRANSCRIPT_HASH: i64 = 0x06;
    pub const GROUP_CONTEXT: i64 = 0x07;
    pub const CONFIRMATION_TAG: i64 = 0x08;
    pub const GROUP_STATE: i64 = 0x09;
    pub const MESSAGE_SECRETS: i64 = 0x0A;
    pub const RESUMPTION_PSK_STORE: i64 = 0x0B;
    pub const OWN_LEAF_NODE_INDEX: i64 = 0x0C;
    pub const GROUP_EPOCH_SECRETS: i64 = 0x0D;
    /// Group-scoped epoch key pair list. `sub_key = cbor(epoch || leaf_index)`.
    pub const EPOCH_KEY_PAIRS: i64 = 0x0E;

    // Unscoped (group_id == empty blob).
    pub const SIGNATURE_KEY_PAIR: i64 = 0x20;
    pub const ENCRYPTION_KEY_PAIR: i64 = 0x21;
    pub const KEY_PACKAGE: i64 = 0x22;
    pub const PSK: i64 = 0x23;
}

type Result<T> = std::result::Result<T, PromtuzMlsStorageError>;

/// rusqlite-backed `OpenMlsProvider::StorageProvider`.
///
/// Holds an `Arc<Mutex<Connection>>`. The mutex is `parking_lot::Mutex`
/// (matching the rest of libcore — see `db/macros.rs`); it is *never*
/// held across an `await` because the storage trait is synchronous.
#[derive(Clone)]
pub struct PromtuzStorageProvider {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for PromtuzStorageProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromtuzStorageProvider").finish()
    }
}

impl PromtuzStorageProvider {
    /// Build a provider over a caller-supplied SQLite connection.
    ///
    /// The caller is responsible for having applied the MLS schema
    /// migrations (`db::mls::apply_mls_migrations`) on the connection
    /// first; passing a raw connection without migrations will surface
    /// `Sqlite(NoSuchTable)` errors on the first read or write.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    // ---- internals ---------------------------------------------------

    /// CBOR-encode a value, mapping the encoder error onto our enum.
    fn encode<V: Serialize>(value: &V) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(value, &mut buf)
            .map_err(PromtuzMlsStorageError::encode)?;
        Ok(buf)
    }

    fn decode<V: DeserializeOwned>(bytes: &[u8]) -> Result<V> {
        ciborium::de::from_reader(bytes).map_err(PromtuzMlsStorageError::decode)
    }

    /// Write a single `(group_id, key_tag, sub_key) -> value` row.
    /// Enforces the per-`group_id` budget for non-empty `group_id`s.
    ///
    /// Phase 8 (P1 #26): keeps the `mls_group_size` sidecar in sync
    /// transactionally so subsequent `check_budget` calls are a
    /// single-row lookup instead of `SUM(length(value))`.
    fn put(
        &self,
        group_id: &[u8],
        key_tag: i64,
        sub_key: &[u8],
        value: Vec<u8>,
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prev_len: i64 = tx
            .query_row(
                "SELECT length(value) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                params![group_id, key_tag, sub_key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if !group_id.is_empty() {
            Self::check_budget(&tx, group_id, value.len() as i64 - prev_len)?;
        }
        let new_len = value.len() as i64;
        tx.execute(
            "INSERT INTO mls_storage(group_id, key_tag, sub_key, value) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(group_id, key_tag, sub_key) DO UPDATE SET value = excluded.value",
            params![group_id, key_tag, sub_key, value],
        )?;
        if !group_id.is_empty() {
            Self::add_to_group_size(&tx, group_id, new_len - prev_len)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Compute the projected post-write size from the sidecar table
    /// and return `BudgetExceeded` if it would breach
    /// `MLS_GROUP_STATE_BUDGET_BYTES`.
    ///
    /// **Phase 8 (P1 #26)**: takes a *delta* (this write's
    /// `new_len - prev_len`) so the caller doesn't double-count
    /// when the row is being replaced. Internally consults
    /// `mls_group_size` (single-row lookup) instead of the prior
    /// `SUM(length(value))` full-table scan.
    fn check_budget(
        conn: &Connection,
        group_id: &[u8],
        delta: i64,
    ) -> Result<()> {
        let existing: i64 = conn
            .query_row(
                "SELECT total_bytes FROM mls_group_size WHERE group_id = ?1",
                params![group_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let projected = existing + delta;
        if projected < 0 {
            // Defensive: a negative projected size means the sidecar
            // got out of sync with the real table. Fall back to the
            // SUM-based check rather than silently accept.
            let actual: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(length(value)), 0) FROM mls_storage \
                     WHERE group_id = ?1",
                    params![group_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if (actual + delta) as u64 > MLS_GROUP_STATE_BUDGET_BYTES {
                return Err(PromtuzMlsStorageError::BudgetExceeded {
                    existing: actual as u64,
                    requested: delta.max(0) as u64,
                    limit: MLS_GROUP_STATE_BUDGET_BYTES,
                });
            }
            return Ok(());
        }
        if projected as u64 > MLS_GROUP_STATE_BUDGET_BYTES {
            return Err(PromtuzMlsStorageError::BudgetExceeded {
                existing: existing as u64,
                requested: delta.max(0) as u64,
                limit: MLS_GROUP_STATE_BUDGET_BYTES,
            });
        }
        Ok(())
    }

    /// Phase 8 (P1 #26): apply a signed delta to the
    /// `mls_group_size` sidecar row. Caller is inside a transaction.
    fn add_to_group_size(
        conn: &Connection, group_id: &[u8], delta: i64,
    ) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        // INSERT-OR-REPLACE arithmetic: if the row exists, total = old + delta;
        // otherwise insert the delta directly (clamped at 0).
        conn.execute(
            "INSERT INTO mls_group_size(group_id, total_bytes) \
             VALUES (?1, MAX(?2, 0)) \
             ON CONFLICT(group_id) DO UPDATE SET total_bytes = MAX(total_bytes + ?2, 0)",
            params![group_id, delta],
        )?;
        Ok(())
    }

    /// Read the single row at `(group_id, key_tag, sub_key)`, or `None`.
    fn get_raw(
        &self,
        group_id: &[u8],
        key_tag: i64,
        sub_key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT value FROM mls_storage \
             WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
        )?;
        match stmt.query_row(params![group_id, key_tag, sub_key], |r| {
            r.get::<_, Vec<u8>>(0)
        }) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete a single row. No-op if the row doesn't exist.
    /// Phase 8 (P1 #26): sidecar size update.
    fn delete_one(&self, group_id: &[u8], key_tag: i64, sub_key: &[u8]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prev_len: i64 = tx
            .query_row(
                "SELECT length(value) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                params![group_id, key_tag, sub_key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        tx.execute(
            "DELETE FROM mls_storage \
             WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
            params![group_id, key_tag, sub_key],
        )?;
        if !group_id.is_empty() && prev_len > 0 {
            Self::add_to_group_size(&tx, group_id, -prev_len)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every row with `(group_id, key_tag)`, regardless of `sub_key`.
    /// Used for proposal-queue clearing.
    /// Phase 8 (P1 #26): sidecar size update (sums the deleted lengths
    /// and subtracts in one go).
    fn delete_by_tag(&self, group_id: &[u8], key_tag: i64) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let total_to_remove: i64 = tx
            .query_row(
                "SELECT COALESCE(SUM(length(value)), 0) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2",
                params![group_id, key_tag],
                |r| r.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "DELETE FROM mls_storage WHERE group_id = ?1 AND key_tag = ?2",
            params![group_id, key_tag],
        )?;
        if !group_id.is_empty() && total_to_remove > 0 {
            Self::add_to_group_size(&tx, group_id, -total_to_remove)?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- list helpers (proposal refs, own leaf nodes) --------------

    // -------------------------------------------------------------
    // Phase 8 (P1 #25): per-element list rows.
    //
    // Layout: each list element lives in its own row keyed
    // `(group_id, key_tag, sub_key = u64_be(list_index))`. Append is
    // a single INSERT (with `next_index = 1 + max(sub_key)` looked up
    // inside the transaction); remove is `DELETE WHERE value = ?`;
    // read is `SELECT value … ORDER BY sub_key ASC`.
    //
    // Trade-off: indices are not reused on remove (new appends always
    // use `max+1`), so a long-running list may end up with sparse
    // indices. Acceptable: u64_be is 8 bytes and the proposal queue
    // is bounded by the commit cadence; no monotonically-growing
    // unbounded process here.
    //
    // The previous CBOR-blob layout from earlier phases is wiped by
    // a Phase 8 migration (`db::mls`).
    // -------------------------------------------------------------

    /// Append `value` to the per-element list rows for `(group_id, key_tag)`.
    fn list_append(&self, group_id: &[u8], key_tag: i64, value: Vec<u8>) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Find next index = max(sub_key)+1, or 0 if the list is empty.
        let max_idx: Option<Vec<u8>> = tx
            .query_row(
                "SELECT MAX(sub_key) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2",
                params![group_id, key_tag],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let next_idx: u64 = max_idx
            .as_ref()
            .and_then(|b| {
                if b.len() == 8 {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(b);
                    Some(u64::from_be_bytes(a) + 1)
                } else {
                    None
                }
            })
            .unwrap_or(0);

        let sub_key = next_idx.to_be_bytes();
        if !group_id.is_empty() {
            Self::check_budget(&tx, group_id, value.len() as i64)?;
        }
        tx.execute(
            "INSERT INTO mls_storage(group_id, key_tag, sub_key, value) \
             VALUES (?1, ?2, ?3, ?4)",
            params![group_id, key_tag, &sub_key as &[u8], &value],
        )?;
        // Phase 8 (P1 #26): sidecar size accounting.
        if !group_id.is_empty() {
            Self::add_to_group_size(&tx, group_id, value.len() as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the list in insertion order.
    fn list_read<V: DeserializeOwned>(
        &self,
        group_id: &[u8],
        key_tag: i64,
    ) -> Result<Vec<V>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT value FROM mls_storage \
             WHERE group_id = ?1 AND key_tag = ?2 \
             ORDER BY sub_key ASC",
        )?;
        let rows = stmt
            .query_map(params![group_id, key_tag], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.iter().map(|b| Self::decode(b)).collect()
    }

    /// Remove the first occurrence of `value` from the list. No-op if
    /// the list (or the value within it) doesn't exist.
    fn list_remove(&self, group_id: &[u8], key_tag: i64, value: &[u8]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

        // Find the lowest-indexed row whose value matches. Single-row
        // delete keeps semantics aligned with the prior `Vec::remove`
        // behaviour (drop the first match only).
        let row: Option<(Vec<u8>, i64)> = tx
            .query_row(
                "SELECT sub_key, length(value) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND value = ?3 \
                 ORDER BY sub_key ASC LIMIT 1",
                params![group_id, key_tag, value],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((sub_key, prev_len)) = row {
            tx.execute(
                "DELETE FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                params![group_id, key_tag, &sub_key as &[u8]],
            )?;
            if !group_id.is_empty() {
                Self::add_to_group_size(&tx, group_id, -prev_len)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ---- key encoding ------------------------------------------------

    fn encode_group_id<G: Serialize>(group_id: &G) -> Result<Vec<u8>> {
        Self::encode(group_id)
    }
    fn encode_key<K: Serialize>(key: &K) -> Result<Vec<u8>> {
        Self::encode(key)
    }
}

// =====================================================================
// StorageProvider impl
// =====================================================================

impl StorageProvider<CURRENT_VERSION> for PromtuzStorageProvider {
    type Error = PromtuzMlsStorageError;

    // ---- writers (group-state) ---------------------------------------

    fn write_mls_join_config<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        MlsGroupJoinConfig: traits::MlsGroupJoinConfig<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        config: &MlsGroupJoinConfig,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(config)?;
        self.put(&gid, tags::JOIN_CONFIG, &[], value)
    }

    fn append_own_leaf_node<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        LeafNode: traits::LeafNode<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        leaf_node: &LeafNode,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(leaf_node)?;
        self.list_append(&gid, tags::OWN_LEAF_NODES, value)
    }

    fn queue_proposal<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
        QueuedProposal: traits::QueuedProposal<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        proposal_ref: &ProposalRef,
        proposal: &QueuedProposal,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let pref_bytes = Self::encode_key(proposal_ref)?;

        // 1. Store proposal under (gid, QUEUED_PROPOSAL, pref_bytes).
        let value = Self::encode(proposal)?;
        self.put(&gid, tags::QUEUED_PROPOSAL, &pref_bytes, value)?;

        // 2. Append pref to the queue list.
        self.list_append(&gid, tags::PROPOSAL_QUEUE_REFS, pref_bytes)
    }

    fn write_tree<GroupId: traits::GroupId<CURRENT_VERSION>, TreeSync: traits::TreeSync<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
        tree: &TreeSync,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(tree)?;
        self.put(&gid, tags::TREE, &[], value)
    }

    fn write_interim_transcript_hash<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        InterimTranscriptHash: traits::InterimTranscriptHash<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        interim_transcript_hash: &InterimTranscriptHash,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(interim_transcript_hash)?;
        self.put(&gid, tags::INTERIM_TRANSCRIPT_HASH, &[], value)
    }

    fn write_context<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        GroupContext: traits::GroupContext<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        group_context: &GroupContext,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(group_context)?;
        self.put(&gid, tags::GROUP_CONTEXT, &[], value)
    }

    fn write_confirmation_tag<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ConfirmationTag: traits::ConfirmationTag<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        confirmation_tag: &ConfirmationTag,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(confirmation_tag)?;
        self.put(&gid, tags::CONFIRMATION_TAG, &[], value)
    }

    fn write_group_state<
        GroupState: traits::GroupState<CURRENT_VERSION>,
        GroupId: traits::GroupId<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        group_state: &GroupState,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(group_state)?;
        self.put(&gid, tags::GROUP_STATE, &[], value)
    }

    fn write_message_secrets<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        MessageSecrets: traits::MessageSecrets<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        message_secrets: &MessageSecrets,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(message_secrets)?;
        self.put(&gid, tags::MESSAGE_SECRETS, &[], value)
    }

    fn write_resumption_psk_store<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ResumptionPskStore: traits::ResumptionPskStore<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        resumption_psk_store: &ResumptionPskStore,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(resumption_psk_store)?;
        self.put(&gid, tags::RESUMPTION_PSK_STORE, &[], value)
    }

    fn write_own_leaf_index<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        LeafNodeIndex: traits::LeafNodeIndex<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        own_leaf_index: &LeafNodeIndex,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(own_leaf_index)?;
        self.put(&gid, tags::OWN_LEAF_NODE_INDEX, &[], value)
    }

    fn write_group_epoch_secrets<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        GroupEpochSecrets: traits::GroupEpochSecrets<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        group_epoch_secrets: &GroupEpochSecrets,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let value = Self::encode(group_epoch_secrets)?;
        self.put(&gid, tags::GROUP_EPOCH_SECRETS, &[], value)
    }

    // ---- writers (crypto objects) ------------------------------------

    fn write_signature_key_pair<
        SignaturePublicKey: traits::SignaturePublicKey<CURRENT_VERSION>,
        SignatureKeyPair: traits::SignatureKeyPair<CURRENT_VERSION>,
    >(
        &self,
        public_key: &SignaturePublicKey,
        signature_key_pair: &SignatureKeyPair,
    ) -> Result<()> {
        let pk = Self::encode_key(public_key)?;
        let value = Self::encode(signature_key_pair)?;
        self.put(&[], tags::SIGNATURE_KEY_PAIR, &pk, value)
    }

    fn write_encryption_key_pair<
        EncryptionKey: traits::EncryptionKey<CURRENT_VERSION>,
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
    >(
        &self,
        public_key: &EncryptionKey,
        key_pair: &HpkeKeyPair,
    ) -> Result<()> {
        let pk = Self::encode_key(public_key)?;
        let value = Self::encode(key_pair)?;
        self.put(&[], tags::ENCRYPTION_KEY_PAIR, &pk, value)
    }

    fn write_encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        epoch: &EpochKey,
        leaf_index: u32,
        key_pairs: &[HpkeKeyPair],
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let sub_key = epoch_leaf_subkey(epoch, leaf_index)?;
        // serde's slice `Serialize` impl does not require T: Clone.
        let value = Self::encode(&key_pairs)?;
        self.put(&gid, tags::EPOCH_KEY_PAIRS, &sub_key, value)
    }

    fn write_key_package<
        HashReference: traits::HashReference<CURRENT_VERSION>,
        KeyPackage: traits::KeyPackage<CURRENT_VERSION>,
    >(
        &self,
        hash_ref: &HashReference,
        key_package: &KeyPackage,
    ) -> Result<()> {
        let href = Self::encode_key(hash_ref)?;
        let value = Self::encode(key_package)?;
        self.put(&[], tags::KEY_PACKAGE, &href, value)
    }

    fn write_psk<PskId: traits::PskId<CURRENT_VERSION>, PskBundle: traits::PskBundle<CURRENT_VERSION>>(
        &self,
        psk_id: &PskId,
        psk: &PskBundle,
    ) -> Result<()> {
        let psk_id_bytes = Self::encode_key(psk_id)?;
        let value = Self::encode(psk)?;
        self.put(&[], tags::PSK, &psk_id_bytes, value)
    }

    // ---- getters (group-state) ---------------------------------------

    fn mls_group_join_config<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        MlsGroupJoinConfig: traits::MlsGroupJoinConfig<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<MlsGroupJoinConfig>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::JOIN_CONFIG, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn own_leaf_nodes<GroupId: traits::GroupId<CURRENT_VERSION>, LeafNode: traits::LeafNode<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<Vec<LeafNode>> {
        let gid = Self::encode_group_id(group_id)?;
        self.list_read(&gid, tags::OWN_LEAF_NODES)
    }

    fn queued_proposal_refs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Vec<ProposalRef>> {
        let gid = Self::encode_group_id(group_id)?;
        self.list_read(&gid, tags::PROPOSAL_QUEUE_REFS)
    }

    fn queued_proposals<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
        QueuedProposal: traits::QueuedProposal<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Vec<(ProposalRef, QueuedProposal)>> {
        let gid = Self::encode_group_id(group_id)?;
        // Phase 8 (P1 #25): the proposal-ref list is now stored as
        // multiple per-row entries (one row per index). Read the raw
        // CBOR bytes for each ref; use them both to decode the
        // public ProposalRef and as the sub_key for the per-proposal
        // payload lookup.
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT value FROM mls_storage \
             WHERE group_id = ?1 AND key_tag = ?2 \
             ORDER BY sub_key ASC",
        )?;
        let pref_bytes_list: Vec<Vec<u8>> = stmt
            .query_map(params![&gid, tags::PROPOSAL_QUEUE_REFS], |r| {
                r.get::<_, Vec<u8>>(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        drop(conn);

        let mut out = Vec::with_capacity(pref_bytes_list.len());
        for pref_bytes in pref_bytes_list {
            let pref: ProposalRef = Self::decode(&pref_bytes)?;
            let prop_raw = self
                .get_raw(&gid, tags::QUEUED_PROPOSAL, &pref_bytes)?
                .ok_or_else(|| {
                    PromtuzMlsStorageError::decode("queued proposal missing for known ref")
                })?;
            let proposal: QueuedProposal = Self::decode(&prop_raw)?;
            out.push((pref, proposal));
        }
        Ok(out)
    }

    fn tree<GroupId: traits::GroupId<CURRENT_VERSION>, TreeSync: traits::TreeSync<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<TreeSync>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::TREE, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn group_context<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        GroupContext: traits::GroupContext<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<GroupContext>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::GROUP_CONTEXT, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn interim_transcript_hash<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        InterimTranscriptHash: traits::InterimTranscriptHash<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<InterimTranscriptHash>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::INTERIM_TRANSCRIPT_HASH, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn confirmation_tag<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ConfirmationTag: traits::ConfirmationTag<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<ConfirmationTag>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::CONFIRMATION_TAG, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn group_state<GroupState: traits::GroupState<CURRENT_VERSION>, GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<GroupState>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::GROUP_STATE, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn message_secrets<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        MessageSecrets: traits::MessageSecrets<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<MessageSecrets>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::MESSAGE_SECRETS, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn resumption_psk_store<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ResumptionPskStore: traits::ResumptionPskStore<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<ResumptionPskStore>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::RESUMPTION_PSK_STORE, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn own_leaf_index<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        LeafNodeIndex: traits::LeafNodeIndex<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<LeafNodeIndex>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::OWN_LEAF_NODE_INDEX, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn group_epoch_secrets<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        GroupEpochSecrets: traits::GroupEpochSecrets<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<Option<GroupEpochSecrets>> {
        let gid = Self::encode_group_id(group_id)?;
        match self.get_raw(&gid, tags::GROUP_EPOCH_SECRETS, &[])? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    // ---- getters (crypto objects) ------------------------------------

    fn signature_key_pair<
        SignaturePublicKey: traits::SignaturePublicKey<CURRENT_VERSION>,
        SignatureKeyPair: traits::SignatureKeyPair<CURRENT_VERSION>,
    >(
        &self,
        public_key: &SignaturePublicKey,
    ) -> Result<Option<SignatureKeyPair>> {
        let pk = Self::encode_key(public_key)?;
        match self.get_raw(&[], tags::SIGNATURE_KEY_PAIR, &pk)? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn encryption_key_pair<
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
        EncryptionKey: traits::EncryptionKey<CURRENT_VERSION>,
    >(
        &self,
        public_key: &EncryptionKey,
    ) -> Result<Option<HpkeKeyPair>> {
        let pk = Self::encode_key(public_key)?;
        match self.get_raw(&[], tags::ENCRYPTION_KEY_PAIR, &pk)? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        epoch: &EpochKey,
        leaf_index: u32,
    ) -> Result<Vec<HpkeKeyPair>> {
        let gid = Self::encode_group_id(group_id)?;
        let sub_key = epoch_leaf_subkey(epoch, leaf_index)?;
        match self.get_raw(&gid, tags::EPOCH_KEY_PAIRS, &sub_key)? {
            Some(b) => Self::decode(&b),
            None => Ok(Vec::new()),
        }
    }

    fn key_package<
        KeyPackageRef: traits::HashReference<CURRENT_VERSION>,
        KeyPackage: traits::KeyPackage<CURRENT_VERSION>,
    >(
        &self,
        hash_ref: &KeyPackageRef,
    ) -> Result<Option<KeyPackage>> {
        let href = Self::encode_key(hash_ref)?;
        match self.get_raw(&[], tags::KEY_PACKAGE, &href)? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    fn psk<PskBundle: traits::PskBundle<CURRENT_VERSION>, PskId: traits::PskId<CURRENT_VERSION>>(
        &self,
        psk_id: &PskId,
    ) -> Result<Option<PskBundle>> {
        let psk_id_bytes = Self::encode_key(psk_id)?;
        match self.get_raw(&[], tags::PSK, &psk_id_bytes)? {
            Some(b) => Self::decode(&b).map(Some),
            None => Ok(None),
        }
    }

    // ---- deleters (group-state) --------------------------------------

    fn remove_proposal<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        proposal_ref: &ProposalRef,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let pref_bytes = Self::encode_key(proposal_ref)?;

        // Drop the queued proposal payload.
        self.delete_one(&gid, tags::QUEUED_PROPOSAL, &pref_bytes)?;
        // And remove the ref from the queue list.
        self.list_remove(&gid, tags::PROPOSAL_QUEUE_REFS, &pref_bytes)
    }

    // Phase 8 (P1 #25): list-typed entries now live as multiple
    // rows under different sub_keys; use `delete_by_tag` to drop
    // them all in one go. Same change for `clear_proposal_queue`
    // below.
    fn delete_own_leaf_nodes<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        // Phase 8 (P1 #25): list elements live as multiple per-row
        // entries; drop the whole list with `delete_by_tag`.
        self.delete_by_tag(&gid, tags::OWN_LEAF_NODES)
    }

    fn delete_group_config<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::JOIN_CONFIG, &[])
    }

    fn delete_tree<GroupId: traits::GroupId<CURRENT_VERSION>>(&self, group_id: &GroupId) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::TREE, &[])
    }

    fn delete_confirmation_tag<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::CONFIRMATION_TAG, &[])
    }

    fn delete_group_state<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::GROUP_STATE, &[])
    }

    fn delete_context<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::GROUP_CONTEXT, &[])
    }

    fn delete_interim_transcript_hash<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::INTERIM_TRANSCRIPT_HASH, &[])
    }

    fn delete_message_secrets<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::MESSAGE_SECRETS, &[])
    }

    fn delete_all_resumption_psk_secrets<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::RESUMPTION_PSK_STORE, &[])
    }

    fn delete_own_leaf_index<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::OWN_LEAF_NODE_INDEX, &[])
    }

    fn delete_group_epoch_secrets<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        self.delete_one(&gid, tags::GROUP_EPOCH_SECRETS, &[])
    }

    fn clear_proposal_queue<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        // Drop every queued proposal payload (any sub_key) for this group.
        self.delete_by_tag(&gid, tags::QUEUED_PROPOSAL)?;
        // Phase 8 (P1 #25): proposal-ref list elements live as
        // multiple per-row entries; `delete_by_tag` drops them all.
        self.delete_by_tag(&gid, tags::PROPOSAL_QUEUE_REFS)
    }

    // ---- deleters (crypto objects) -----------------------------------

    fn delete_signature_key_pair<SignaturePublicKey: traits::SignaturePublicKey<CURRENT_VERSION>>(
        &self,
        public_key: &SignaturePublicKey,
    ) -> Result<()> {
        let pk = Self::encode_key(public_key)?;
        self.delete_one(&[], tags::SIGNATURE_KEY_PAIR, &pk)
    }

    fn delete_encryption_key_pair<EncryptionKey: traits::EncryptionKey<CURRENT_VERSION>>(
        &self,
        public_key: &EncryptionKey,
    ) -> Result<()> {
        let pk = Self::encode_key(public_key)?;
        self.delete_one(&[], tags::ENCRYPTION_KEY_PAIR, &pk)
    }

    fn delete_encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
    >(
        &self,
        group_id: &GroupId,
        epoch: &EpochKey,
        leaf_index: u32,
    ) -> Result<()> {
        let gid = Self::encode_group_id(group_id)?;
        let sub_key = epoch_leaf_subkey(epoch, leaf_index)?;
        self.delete_one(&gid, tags::EPOCH_KEY_PAIRS, &sub_key)
    }

    fn delete_key_package<KeyPackageRef: traits::HashReference<CURRENT_VERSION>>(
        &self,
        hash_ref: &KeyPackageRef,
    ) -> Result<()> {
        let href = Self::encode_key(hash_ref)?;
        self.delete_one(&[], tags::KEY_PACKAGE, &href)
    }

    fn delete_psk<PskKey: traits::PskId<CURRENT_VERSION>>(&self, psk_id: &PskKey) -> Result<()> {
        let psk_id_bytes = Self::encode_key(psk_id)?;
        self.delete_one(&[], tags::PSK, &psk_id_bytes)
    }
}

/// Compose the sub_key for `(epoch, leaf_index)` slots.
fn epoch_leaf_subkey<EpochKey: Serialize>(
    epoch: &EpochKey,
    leaf_index: u32,
) -> Result<Vec<u8>> {
    // Encode (epoch, leaf_index) as a CBOR tuple — stable + compact.
    let mut buf = Vec::new();
    ciborium::ser::into_writer(&(epoch, leaf_index), &mut buf)
        .map_err(PromtuzMlsStorageError::encode)?;
    Ok(buf)
}

// =====================================================================
// Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use openmls_traits::storage::Entity;
    use openmls_traits::storage::Key;
    use serde::Deserialize;
    use serde::Serialize;

    /// Stand-in `GroupId` test fixture. Implements the openmls
    /// `Key` + `GroupId` traits so we can drive `StorageProvider`
    /// methods without dragging the full openmls type.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct TestGroupId(Vec<u8>);
    impl Key<CURRENT_VERSION> for TestGroupId {}
    impl traits::GroupId<CURRENT_VERSION> for TestGroupId {}
    impl Entity<CURRENT_VERSION> for TestGroupId {}

    /// Stand-in entity blob so we can test arbitrary value lengths.
    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct TestBlob(Vec<u8>);
    impl Entity<CURRENT_VERSION> for TestBlob {}
    impl traits::GroupContext<CURRENT_VERSION> for TestBlob {}
    impl traits::TreeSync<CURRENT_VERSION> for TestBlob {}
    impl traits::InterimTranscriptHash<CURRENT_VERSION> for TestBlob {}
    impl traits::ConfirmationTag<CURRENT_VERSION> for TestBlob {}
    impl traits::GroupState<CURRENT_VERSION> for TestBlob {}
    impl traits::MessageSecrets<CURRENT_VERSION> for TestBlob {}
    impl traits::ResumptionPskStore<CURRENT_VERSION> for TestBlob {}
    impl traits::LeafNodeIndex<CURRENT_VERSION> for TestBlob {}
    impl traits::GroupEpochSecrets<CURRENT_VERSION> for TestBlob {}
    impl traits::MlsGroupJoinConfig<CURRENT_VERSION> for TestBlob {}
    impl traits::LeafNode<CURRENT_VERSION> for TestBlob {}
    impl traits::QueuedProposal<CURRENT_VERSION> for TestBlob {}
    impl traits::HpkeKeyPair<CURRENT_VERSION> for TestBlob {}
    impl traits::SignatureKeyPair<CURRENT_VERSION> for TestBlob {}
    impl traits::KeyPackage<CURRENT_VERSION> for TestBlob {}
    impl traits::PskBundle<CURRENT_VERSION> for TestBlob {}

    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct TestRef(u64);
    impl Key<CURRENT_VERSION> for TestRef {}
    impl Entity<CURRENT_VERSION> for TestRef {}
    impl traits::ProposalRef<CURRENT_VERSION> for TestRef {}
    impl traits::HashReference<CURRENT_VERSION> for TestRef {}

    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct TestPubKey(Vec<u8>);
    impl Key<CURRENT_VERSION> for TestPubKey {}
    impl traits::SignaturePublicKey<CURRENT_VERSION> for TestPubKey {}
    impl traits::EncryptionKey<CURRENT_VERSION> for TestPubKey {}
    impl traits::PskId<CURRENT_VERSION> for TestPubKey {}

    #[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
    struct TestEpoch(u64);
    impl Key<CURRENT_VERSION> for TestEpoch {}
    impl traits::EpochKey<CURRENT_VERSION> for TestEpoch {}

    fn fresh_provider() -> PromtuzStorageProvider {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_mls_migrations(&mut conn);
        PromtuzStorageProvider::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn group_state_round_trip() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0xDEu8, 0xAD, 0xBE, 0xEF]);
        let blob = TestBlob(vec![0xCAu8; 16]);

        p.write_group_state(&gid, &blob).expect("write");
        let got: Option<TestBlob> = p.group_state(&gid).expect("read");
        assert_eq!(got, Some(blob));
    }

    #[test]
    fn entity_variants_round_trip() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0x01]);
        let blob = TestBlob(vec![0x42; 8]);

        // Group-scoped slot variants.
        p.write_mls_join_config(&gid, &blob).expect("join_config");
        p.write_tree(&gid, &blob).expect("tree");
        p.write_context(&gid, &blob).expect("context");
        p.write_interim_transcript_hash(&gid, &blob).expect("ith");
        p.write_confirmation_tag(&gid, &blob).expect("conf_tag");
        p.write_group_state(&gid, &blob).expect("group_state");
        p.write_message_secrets(&gid, &blob).expect("msg_secrets");
        p.write_resumption_psk_store(&gid, &blob).expect("rps");
        p.write_own_leaf_index(&gid, &blob).expect("oli");
        p.write_group_epoch_secrets(&gid, &blob).expect("ges");

        // Read back via each typed getter, verifying equality.
        assert_eq!(
            p.mls_group_join_config::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(p.tree::<_, TestBlob>(&gid).expect("r"), Some(blob.clone()));
        assert_eq!(
            p.group_context::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.interim_transcript_hash::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.confirmation_tag::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.group_state::<TestBlob, _>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.message_secrets::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.resumption_psk_store::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.own_leaf_index::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.group_epoch_secrets::<_, TestBlob>(&gid).expect("r"),
            Some(blob.clone())
        );

        // Crypto-keyed slots (un-scoped).
        let pubkey = TestPubKey(vec![0xAA; 32]);
        p.write_signature_key_pair(&pubkey, &blob).expect("sig_kp");
        p.write_encryption_key_pair(&pubkey, &blob).expect("enc_kp");
        let kp_ref = TestRef(7);
        p.write_key_package(&kp_ref, &blob).expect("key_package");
        let psk_id = TestPubKey(vec![0xBB; 32]);
        p.write_psk(&psk_id, &blob).expect("psk");

        assert_eq!(
            p.signature_key_pair::<_, TestBlob>(&pubkey).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.encryption_key_pair::<TestBlob, _>(&pubkey).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(
            p.key_package::<_, TestBlob>(&kp_ref).expect("r"),
            Some(blob.clone())
        );
        assert_eq!(p.psk::<TestBlob, _>(&psk_id).expect("r"), Some(blob));
    }

    #[test]
    fn proposal_queue_lifecycle() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0x01]);
        let r1 = TestRef(1);
        let r2 = TestRef(2);
        let prop1 = TestBlob(vec![1, 1, 1]);
        let prop2 = TestBlob(vec![2, 2, 2]);

        p.queue_proposal(&gid, &r1, &prop1).expect("q1");
        p.queue_proposal(&gid, &r2, &prop2).expect("q2");

        let refs: Vec<TestRef> = p.queued_proposal_refs(&gid).expect("refs");
        assert_eq!(refs, vec![r1.clone(), r2.clone()]);

        let entries: Vec<(TestRef, TestBlob)> = p.queued_proposals(&gid).expect("entries");
        assert_eq!(
            entries,
            vec![(r1.clone(), prop1.clone()), (r2.clone(), prop2.clone())]
        );

        // Remove one — list should shrink, the removed proposal should
        // disappear, the survivor stays.
        p.remove_proposal(&gid, &r1).expect("remove");
        let refs: Vec<TestRef> = p.queued_proposal_refs(&gid).expect("refs");
        assert_eq!(refs, vec![r2.clone()]);

        // Clear the queue: both refs and stored proposals must vanish.
        p.clear_proposal_queue::<_, TestRef>(&gid).expect("clear");
        let refs: Vec<TestRef> = p.queued_proposal_refs(&gid).expect("refs");
        assert!(refs.is_empty());
        let entries: Vec<(TestRef, TestBlob)> = p.queued_proposals(&gid).expect("entries");
        assert!(entries.is_empty());
    }

    #[test]
    fn own_leaf_nodes_list_append_and_delete() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0x01]);
        let l1 = TestBlob(vec![1]);
        let l2 = TestBlob(vec![2]);

        p.append_own_leaf_node(&gid, &l1).expect("a1");
        p.append_own_leaf_node(&gid, &l2).expect("a2");
        let got: Vec<TestBlob> = p.own_leaf_nodes(&gid).expect("read");
        assert_eq!(got, vec![l1, l2]);

        p.delete_own_leaf_nodes(&gid).expect("delete");
        let got: Vec<TestBlob> = p.own_leaf_nodes(&gid).expect("read");
        assert!(got.is_empty());
    }

    #[test]
    fn epoch_key_pairs_round_trip_and_delete() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0xFFu8; 8]);
        let epoch = TestEpoch(5);
        let kp1 = TestBlob(vec![1, 2, 3]);
        let kp2 = TestBlob(vec![4, 5, 6]);

        p.write_encryption_epoch_key_pairs(&gid, &epoch, 0, &[kp1.clone(), kp2.clone()])
            .expect("write");
        let got: Vec<TestBlob> = p
            .encryption_epoch_key_pairs(&gid, &epoch, 0)
            .expect("read");
        assert_eq!(got, vec![kp1, kp2]);

        // Different leaf_index addresses a different slot.
        let got: Vec<TestBlob> = p
            .encryption_epoch_key_pairs(&gid, &epoch, 1)
            .expect("read other slot");
        assert!(got.is_empty());

        p.delete_encryption_epoch_key_pairs(&gid, &epoch, 0)
            .expect("delete");
        let got: Vec<TestBlob> = p
            .encryption_epoch_key_pairs(&gid, &epoch, 0)
            .expect("read post-delete");
        assert!(got.is_empty());
    }

    #[test]
    fn delete_then_get_returns_none_for_each_variant() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0xA1]);
        let blob = TestBlob(vec![0; 4]);

        p.write_group_state(&gid, &blob).expect("w");
        p.write_tree(&gid, &blob).expect("w");
        p.write_context(&gid, &blob).expect("w");
        p.write_interim_transcript_hash(&gid, &blob).expect("w");
        p.write_confirmation_tag(&gid, &blob).expect("w");
        p.write_message_secrets(&gid, &blob).expect("w");
        p.write_resumption_psk_store(&gid, &blob).expect("w");
        p.write_own_leaf_index(&gid, &blob).expect("w");
        p.write_group_epoch_secrets(&gid, &blob).expect("w");
        p.write_mls_join_config(&gid, &blob).expect("w");

        p.delete_group_state(&gid).expect("d");
        p.delete_tree(&gid).expect("d");
        p.delete_context(&gid).expect("d");
        p.delete_interim_transcript_hash(&gid).expect("d");
        p.delete_confirmation_tag(&gid).expect("d");
        p.delete_message_secrets(&gid).expect("d");
        p.delete_all_resumption_psk_secrets(&gid).expect("d");
        p.delete_own_leaf_index(&gid).expect("d");
        p.delete_group_epoch_secrets(&gid).expect("d");
        p.delete_group_config(&gid).expect("d");

        assert!(p.group_state::<TestBlob, _>(&gid).expect("r").is_none());
        assert!(p.tree::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p.group_context::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p
            .interim_transcript_hash::<_, TestBlob>(&gid)
            .expect("r")
            .is_none());
        assert!(p.confirmation_tag::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p.message_secrets::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p
            .resumption_psk_store::<_, TestBlob>(&gid)
            .expect("r")
            .is_none());
        assert!(p.own_leaf_index::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p
            .group_epoch_secrets::<_, TestBlob>(&gid)
            .expect("r")
            .is_none());
        assert!(p
            .mls_group_join_config::<_, TestBlob>(&gid)
            .expect("r")
            .is_none());
    }

    #[test]
    fn budget_exceeded_on_oversized_write() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0xDE]);

        // First write: under-budget so it lands.
        let small = TestBlob(vec![0; 1024]);
        p.write_group_state(&gid, &small).expect("under budget");

        // Second write: pushes over MLS_GROUP_STATE_BUDGET_BYTES via a
        // distinct slot — distinct so it doesn't replace the existing
        // group_state. Use a 1.5 MiB tree to force the breach.
        let oversized = TestBlob(vec![0u8; (MLS_GROUP_STATE_BUDGET_BYTES as usize) + 64]);
        let err = p.write_tree(&gid, &oversized).unwrap_err();
        match err {
            PromtuzMlsStorageError::BudgetExceeded { limit, .. } => {
                assert_eq!(limit, MLS_GROUP_STATE_BUDGET_BYTES);
            }
            other => panic!("expected BudgetExceeded, got {:?}", other),
        }

        // Original write still readable; the failed write left no trace.
        assert_eq!(
            p.group_state::<TestBlob, _>(&gid).expect("r"),
            Some(small)
        );
        assert!(p.tree::<_, TestBlob>(&gid).expect("r").is_none());
    }

    #[test]
    fn budget_isolation_across_groups() {
        let p = fresh_provider();
        let g_a = TestGroupId(vec![0xAA]);
        let g_b = TestGroupId(vec![0xBB]);

        // Fill group A close to the budget.
        let near_budget = TestBlob(vec![
            0u8;
            (MLS_GROUP_STATE_BUDGET_BYTES as usize)
                .saturating_sub(2048)
        ]);
        p.write_group_state(&g_a, &near_budget).expect("group A");

        // Group B should still accept a comparably-sized write —
        // budgets are per-group.
        p.write_group_state(&g_b, &near_budget).expect("group B");

        // Overflow on group A still fires.
        let overflow = TestBlob(vec![0u8; 4096]);
        let err = p.write_tree(&g_a, &overflow).unwrap_err();
        assert!(matches!(err, PromtuzMlsStorageError::BudgetExceeded { .. }));
    }

    #[test]
    fn budget_freed_after_delete_allows_new_write() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0xCD]);

        // Land 0.9 MiB.
        let nine_tenths = (MLS_GROUP_STATE_BUDGET_BYTES * 9 / 10) as usize;
        let big = TestBlob(vec![0u8; nine_tenths]);
        p.write_group_state(&gid, &big).expect("big write");

        // Trying a second 0.9 MiB write would breach.
        let bigger = TestBlob(vec![0u8; nine_tenths]);
        let err = p.write_tree(&gid, &bigger).unwrap_err();
        assert!(matches!(err, PromtuzMlsStorageError::BudgetExceeded { .. }));

        // After delete the same bigger write must succeed.
        p.delete_group_state(&gid).expect("delete");
        p.write_tree(&gid, &bigger).expect("post-delete write");
    }

    #[test]
    fn adversarial_group_id_bytes_round_trip() {
        let p = fresh_provider();
        // SQL quote, NUL, high bytes — anything parameter binding handles.
        let gid = TestGroupId(b"\x00'`\xff\xfe;DROP TABLE mls_storage;--".to_vec());
        let blob = TestBlob(vec![0xEE; 16]);

        p.write_group_state(&gid, &blob).expect("write");
        assert_eq!(
            p.group_state::<TestBlob, _>(&gid).expect("read"),
            Some(blob)
        );

        // The table is still alive.
        let count: i64 = p
            .conn
            .lock()
            .query_row("SELECT count(*) FROM mls_storage", [], |r| r.get(0))
            .expect("count");
        assert!(count >= 1);
    }

    #[test]
    fn key_package_and_signature_kp_round_trip_and_delete() {
        let p = fresh_provider();
        let pk = TestPubKey(vec![0x11; 32]);
        let kp = TestBlob(vec![0x22; 64]);
        let kp_ref = TestRef(99);

        p.write_signature_key_pair(&pk, &kp).expect("w sig");
        p.write_encryption_key_pair(&pk, &kp).expect("w enc");
        p.write_key_package(&kp_ref, &kp).expect("w kp");

        assert_eq!(
            p.signature_key_pair::<_, TestBlob>(&pk).expect("r sig"),
            Some(kp.clone())
        );
        assert_eq!(
            p.encryption_key_pair::<TestBlob, _>(&pk).expect("r enc"),
            Some(kp.clone())
        );
        assert_eq!(
            p.key_package::<_, TestBlob>(&kp_ref).expect("r kp"),
            Some(kp.clone())
        );

        p.delete_signature_key_pair(&pk).expect("d sig");
        p.delete_encryption_key_pair(&pk).expect("d enc");
        p.delete_key_package(&kp_ref).expect("d kp");

        assert!(p.signature_key_pair::<_, TestBlob>(&pk).expect("r").is_none());
        assert!(p
            .encryption_key_pair::<TestBlob, _>(&pk)
            .expect("r")
            .is_none());
        assert!(p.key_package::<_, TestBlob>(&kp_ref).expect("r").is_none());
    }

    #[test]
    fn psk_round_trip_and_delete() {
        let p = fresh_provider();
        let psk_id = TestPubKey(vec![0x33; 32]);
        let bundle = TestBlob(vec![0x44; 32]);

        p.write_psk(&psk_id, &bundle).expect("w");
        assert_eq!(
            p.psk::<TestBlob, _>(&psk_id).expect("r"),
            Some(bundle)
        );

        p.delete_psk(&psk_id).expect("d");
        assert!(p.psk::<TestBlob, _>(&psk_id).expect("r").is_none());
    }

    #[test]
    fn missing_value_returns_none_not_error() {
        let p = fresh_provider();
        let gid = TestGroupId(vec![0x01]);
        assert!(p.group_state::<TestBlob, _>(&gid).expect("r").is_none());
        assert!(p.tree::<_, TestBlob>(&gid).expect("r").is_none());
        assert!(p
            .own_leaf_nodes::<_, TestBlob>(&gid)
            .expect("r")
            .is_empty());
        assert!(p
            .queued_proposal_refs::<_, TestRef>(&gid)
            .expect("r")
            .is_empty());
    }
}
