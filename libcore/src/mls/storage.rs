//! openmls storage in `mls_storage`: one CBOR value per row, keyed by the CBOR group id (empty when
//! unscoped), a [`tags`] byte and a sub key. A list slot keeps one row per element, under a
//! big-endian `u64` index so rows sort in order.

use std::cell::Cell;
use std::marker::PhantomData;
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

/// `mls_storage.key_tag` values. Stable on disk: never renumber.
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
    /// `sub_key` is the CBOR `(epoch, leaf_index)` pair.
    pub const EPOCH_KEY_PAIRS: i64 = 0x0E;

    // Unscoped (group_id == empty blob).
    pub const SIGNATURE_KEY_PAIR: i64 = 0x20;
    pub const ENCRYPTION_KEY_PAIR: i64 = 0x21;
    pub const KEY_PACKAGE: i64 = 0x22;
    pub const PSK: i64 = 0x23;
}

type Result<T> = std::result::Result<T, PromtuzMlsStorageError>;

#[derive(Clone)]
pub struct PromtuzStorageProvider {
    conn: Arc<Mutex<Connection>>,
}

thread_local! {
    /// The connection this thread's open [`Operation`] holds, and the mutex it came from, so
    /// storage calls inside the operation join its transaction instead of locking again.
    static OPERATION: Cell<(*const Mutex<Connection>, *const Connection)> =
        const { Cell::new((std::ptr::null(), std::ptr::null())) };
}

/// One transaction held open, with the connection locked, across the storage
/// calls one thread makes. Dropped without [`Self::commit`], it rolls back.
pub(crate) struct Operation {
    guard:          parking_lot::ArcMutexGuard<parking_lot::RawMutex, Connection>,
    committed:      bool,
    _single_thread: PhantomData<*const ()>,
}

impl Operation {
    pub(crate) fn conn(&self) -> &Connection {
        &self.guard
    }

    pub(crate) fn commit(mut self) -> Result<()> {
        self.guard.execute_batch("COMMIT")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.guard.execute_batch("ROLLBACK");
        }
        OPERATION.with(|op| op.set((std::ptr::null(), std::ptr::null())));
    }
}

impl std::fmt::Debug for PromtuzStorageProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromtuzStorageProvider").finish()
    }
}

impl PromtuzStorageProvider {
    pub(crate) fn connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    /// Lock the connection and open a transaction that every storage call on
    /// this thread joins until the [`Operation`] commits or drops.
    pub(crate) fn begin(&self) -> Result<Operation> {
        if self.operation_conn().is_some() {
            return Err(PromtuzMlsStorageError::Nested);
        }
        let guard = self.conn.lock_arc();
        guard.execute_batch("BEGIN IMMEDIATE")?;
        OPERATION.with(|op| op.set((Arc::as_ptr(&self.conn), &*guard as *const Connection)));
        Ok(Operation { guard, committed: false, _single_thread: PhantomData })
    }

    /// Run `f` as one operation, so an OpenMLS call that makes several storage
    /// writes lands whole or not at all. Inside an open operation it joins it.
    pub(crate) fn atomic<T, E: From<PromtuzMlsStorageError>>(
        &self, f: impl FnOnce() -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if self.operation_conn().is_some() {
            return f();
        }
        let operation = self.begin()?;
        let out = f()?;
        operation.commit()?;
        Ok(out)
    }

    /// The connection of the operation this thread holds open, if it is on
    /// this provider's connection.
    fn operation_conn(&self) -> Option<&Connection> {
        let (mutex, conn) = OPERATION.with(|op| op.get());
        if conn.is_null() || !std::ptr::eq(mutex, Arc::as_ptr(&self.conn)) {
            return None;
        }
        // SAFETY: `begin` sets the pointer while holding the mutex, the `Operation` clears it
        // before releasing the mutex, and only this thread reads it.
        Some(unsafe { &*conn })
    }

    /// `f` on the connection: the open operation's, or a fresh lock.
    pub(crate) fn with_conn<T, E: From<rusqlite::Error>>(
        &self, f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if let Some(conn) = self.operation_conn() {
            return f(conn);
        }
        f(&self.conn.lock())
    }

    /// `f` inside a transaction: the open operation's, or one of its own.
    pub(crate) fn with_tx<T, E: From<rusqlite::Error>>(
        &self, f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if let Some(conn) = self.operation_conn() {
            return f(conn);
        }
        let mut guard = self.conn.lock();
        let tx = guard.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_tx(f)
    }

    fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_conn(f)
    }
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    fn encode<V: Serialize>(value: &V) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(value, &mut buf)
            .map_err(PromtuzMlsStorageError::encode)?;
        Ok(buf)
    }

    fn decode<V: DeserializeOwned>(bytes: &[u8]) -> Result<V> {
        ciborium::de::from_reader(bytes).map_err(PromtuzMlsStorageError::decode)
    }

    fn put(
        &self,
        group_id: &[u8],
        key_tag: i64,
        sub_key: &[u8],
        value: Vec<u8>,
    ) -> Result<()> {
        self.write(|conn| {
            let prev_len: i64 = conn
                .query_row(
                    "SELECT length(value) FROM mls_storage \
                     WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                    params![group_id, key_tag, sub_key],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if !group_id.is_empty() {
                Self::check_budget(conn, group_id, value.len() as i64 - prev_len)?;
            }
            let new_len = value.len() as i64;
            conn.execute(
                "INSERT INTO mls_storage(group_id, key_tag, sub_key, value) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(group_id, key_tag, sub_key) DO UPDATE SET value = excluded.value",
                params![group_id, key_tag, sub_key, value],
            )?;
            if !group_id.is_empty() {
                Self::add_to_group_size(conn, group_id, new_len - prev_len)?;
            }
            Ok(())
        })
    }

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
            .optional()?
            .unwrap_or(0);
        let projected = existing + delta;
        if projected < 0 {
            // A negative projection means the sidecar drifted, so recount the rows.
            let actual: i64 = conn.query_row(
                "SELECT COALESCE(SUM(length(value)), 0) FROM mls_storage \
                 WHERE group_id = ?1",
                params![group_id],
                |r| r.get(0),
            )?;
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

    fn add_to_group_size(
        conn: &Connection, group_id: &[u8], delta: i64,
    ) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        conn.execute(
            "INSERT INTO mls_group_size(group_id, total_bytes) \
             VALUES (?1, MAX(?2, 0)) \
             ON CONFLICT(group_id) DO UPDATE SET total_bytes = MAX(total_bytes + ?2, 0)",
            params![group_id, delta],
        )?;
        Ok(())
    }

    fn get<V: DeserializeOwned>(
        &self, group_id: &[u8], key_tag: i64, sub_key: &[u8],
    ) -> Result<Option<V>> {
        let value: Option<Vec<u8>> = self.read(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT value FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
            )?;
            Ok(stmt.query_row(params![group_id, key_tag, sub_key], |r| r.get(0)).optional()?)
        })?;
        value.map(|b| Self::decode(&b)).transpose()
    }

    fn delete_one(&self, group_id: &[u8], key_tag: i64, sub_key: &[u8]) -> Result<()> {
        self.write(|conn| {
            let prev_len: i64 = conn
                .query_row(
                    "SELECT length(value) FROM mls_storage \
                     WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                    params![group_id, key_tag, sub_key],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            conn.execute(
                "DELETE FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                params![group_id, key_tag, sub_key],
            )?;
            if !group_id.is_empty() && prev_len > 0 {
                Self::add_to_group_size(conn, group_id, -prev_len)?;
            }
            Ok(())
        })
    }

    fn delete_by_tag(&self, group_id: &[u8], key_tag: i64) -> Result<()> {
        self.write(|conn| {
            let total_to_remove: i64 = conn.query_row(
                "SELECT COALESCE(SUM(length(value)), 0) FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2",
                params![group_id, key_tag],
                |r| r.get(0),
            )?;
            conn.execute(
                "DELETE FROM mls_storage WHERE group_id = ?1 AND key_tag = ?2",
                params![group_id, key_tag],
            )?;
            if !group_id.is_empty() && total_to_remove > 0 {
                Self::add_to_group_size(conn, group_id, -total_to_remove)?;
            }
            Ok(())
        })
    }

    fn list_append(&self, group_id: &[u8], key_tag: i64, value: Vec<u8>) -> Result<()> {
        self.write(|conn| {
            let max_idx: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT MAX(sub_key) FROM mls_storage \
                     WHERE group_id = ?1 AND key_tag = ?2",
                    params![group_id, key_tag],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let next_idx: u64 = max_idx
                .as_deref()
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map_or(0, |a| u64::from_be_bytes(a) + 1);

            let sub_key = next_idx.to_be_bytes();
            if !group_id.is_empty() {
                Self::check_budget(conn, group_id, value.len() as i64)?;
            }
            conn.execute(
                "INSERT INTO mls_storage(group_id, key_tag, sub_key, value) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![group_id, key_tag, &sub_key as &[u8], &value],
            )?;
            if !group_id.is_empty() {
                Self::add_to_group_size(conn, group_id, value.len() as i64)?;
            }
            Ok(())
        })
    }

    fn list_read<V: DeserializeOwned>(
        &self,
        group_id: &[u8],
        key_tag: i64,
    ) -> Result<Vec<V>> {
        self.list_raw(group_id, key_tag)?.iter().map(|b| Self::decode(b)).collect()
    }

    fn list_raw(&self, group_id: &[u8], key_tag: i64) -> Result<Vec<Vec<u8>>> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT value FROM mls_storage \
                 WHERE group_id = ?1 AND key_tag = ?2 \
                 ORDER BY sub_key ASC",
            )?;
            Ok(stmt
                .query_map(params![group_id, key_tag], |r| r.get::<_, Vec<u8>>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    fn list_remove(&self, group_id: &[u8], key_tag: i64, value: &[u8]) -> Result<()> {
        self.write(|conn| {
            let row: Option<(Vec<u8>, i64)> = conn
                .query_row(
                    "SELECT sub_key, length(value) FROM mls_storage \
                     WHERE group_id = ?1 AND key_tag = ?2 AND value = ?3 \
                     ORDER BY sub_key ASC LIMIT 1",
                    params![group_id, key_tag, value],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((sub_key, prev_len)) = row {
                conn.execute(
                    "DELETE FROM mls_storage \
                     WHERE group_id = ?1 AND key_tag = ?2 AND sub_key = ?3",
                    params![group_id, key_tag, &sub_key as &[u8]],
                )?;
                if !group_id.is_empty() {
                    Self::add_to_group_size(conn, group_id, -prev_len)?;
                }
            }
            Ok(())
        })
    }
}

/// `K` is the key's type and `V` the value's, in the trait's generic order. A group row sits under
/// the encoded group id; a `global` row sits under the empty group id, with the key as sub key.
macro_rules! row {
    (put $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K, value: &V) -> Result<()> {
            self.put(&Self::encode(key)?, tags::$tag, &[], Self::encode(value)?)
        }
    };
    (put global $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K, value: &V) -> Result<()> {
            self.put(&[], tags::$tag, &Self::encode(key)?, Self::encode(value)?)
        }
    };
    (get $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K) -> Result<Option<V>> {
            self.get(&Self::encode(key)?, tags::$tag, &[])
        }
    };
    (get global $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K) -> Result<Option<V>> {
            self.get(&[], tags::$tag, &Self::encode(key)?)
        }
    };
    (delete $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K) -> Result<()> {
            self.delete_one(&Self::encode(key)?, tags::$tag, &[])
        }
    };
    (delete global $name:ident<$($g:ident: $t:ident),+> => $tag:ident) => {
        fn $name<$($g: traits::$t<CURRENT_VERSION>),+>(&self, key: &K) -> Result<()> {
            self.delete_one(&[], tags::$tag, &Self::encode(key)?)
        }
    };
}

impl StorageProvider<CURRENT_VERSION> for PromtuzStorageProvider {
    type Error = PromtuzMlsStorageError;

    row!(put write_mls_join_config<K: GroupId, V: MlsGroupJoinConfig> => JOIN_CONFIG);
    row!(put write_tree<K: GroupId, V: TreeSync> => TREE);
    row!(put write_interim_transcript_hash<K: GroupId, V: InterimTranscriptHash> => INTERIM_TRANSCRIPT_HASH);
    row!(put write_context<K: GroupId, V: GroupContext> => GROUP_CONTEXT);
    row!(put write_confirmation_tag<K: GroupId, V: ConfirmationTag> => CONFIRMATION_TAG);
    row!(put write_group_state<V: GroupState, K: GroupId> => GROUP_STATE);
    row!(put write_message_secrets<K: GroupId, V: MessageSecrets> => MESSAGE_SECRETS);
    row!(put write_resumption_psk_store<K: GroupId, V: ResumptionPskStore> => RESUMPTION_PSK_STORE);
    row!(put write_own_leaf_index<K: GroupId, V: LeafNodeIndex> => OWN_LEAF_NODE_INDEX);
    row!(put write_group_epoch_secrets<K: GroupId, V: GroupEpochSecrets> => GROUP_EPOCH_SECRETS);
    row!(put global write_signature_key_pair<K: SignaturePublicKey, V: SignatureKeyPair> => SIGNATURE_KEY_PAIR);
    row!(put global write_encryption_key_pair<K: EncryptionKey, V: HpkeKeyPair> => ENCRYPTION_KEY_PAIR);
    row!(put global write_key_package<K: HashReference, V: KeyPackage> => KEY_PACKAGE);
    row!(put global write_psk<K: PskId, V: PskBundle> => PSK);

    row!(get mls_group_join_config<K: GroupId, V: MlsGroupJoinConfig> => JOIN_CONFIG);
    row!(get tree<K: GroupId, V: TreeSync> => TREE);
    row!(get interim_transcript_hash<K: GroupId, V: InterimTranscriptHash> => INTERIM_TRANSCRIPT_HASH);
    row!(get group_context<K: GroupId, V: GroupContext> => GROUP_CONTEXT);
    row!(get confirmation_tag<K: GroupId, V: ConfirmationTag> => CONFIRMATION_TAG);
    row!(get group_state<V: GroupState, K: GroupId> => GROUP_STATE);
    row!(get message_secrets<K: GroupId, V: MessageSecrets> => MESSAGE_SECRETS);
    row!(get resumption_psk_store<K: GroupId, V: ResumptionPskStore> => RESUMPTION_PSK_STORE);
    row!(get own_leaf_index<K: GroupId, V: LeafNodeIndex> => OWN_LEAF_NODE_INDEX);
    row!(get group_epoch_secrets<K: GroupId, V: GroupEpochSecrets> => GROUP_EPOCH_SECRETS);
    row!(get global signature_key_pair<K: SignaturePublicKey, V: SignatureKeyPair> => SIGNATURE_KEY_PAIR);
    row!(get global encryption_key_pair<V: HpkeKeyPair, K: EncryptionKey> => ENCRYPTION_KEY_PAIR);
    row!(get global key_package<K: HashReference, V: KeyPackage> => KEY_PACKAGE);
    row!(get global psk<V: PskBundle, K: PskId> => PSK);

    row!(delete delete_group_config<K: GroupId> => JOIN_CONFIG);
    row!(delete delete_tree<K: GroupId> => TREE);
    row!(delete delete_interim_transcript_hash<K: GroupId> => INTERIM_TRANSCRIPT_HASH);
    row!(delete delete_context<K: GroupId> => GROUP_CONTEXT);
    row!(delete delete_confirmation_tag<K: GroupId> => CONFIRMATION_TAG);
    row!(delete delete_group_state<K: GroupId> => GROUP_STATE);
    row!(delete delete_message_secrets<K: GroupId> => MESSAGE_SECRETS);
    row!(delete delete_all_resumption_psk_secrets<K: GroupId> => RESUMPTION_PSK_STORE);
    row!(delete delete_own_leaf_index<K: GroupId> => OWN_LEAF_NODE_INDEX);
    row!(delete delete_group_epoch_secrets<K: GroupId> => GROUP_EPOCH_SECRETS);
    row!(delete global delete_signature_key_pair<K: SignaturePublicKey> => SIGNATURE_KEY_PAIR);
    row!(delete global delete_encryption_key_pair<K: EncryptionKey> => ENCRYPTION_KEY_PAIR);
    row!(delete global delete_key_package<K: HashReference> => KEY_PACKAGE);
    row!(delete global delete_psk<K: PskId> => PSK);

    fn append_own_leaf_node<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        LeafNode: traits::LeafNode<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, leaf_node: &LeafNode,
    ) -> Result<()> {
        self.list_append(&Self::encode(group_id)?, tags::OWN_LEAF_NODES, Self::encode(leaf_node)?)
    }

    fn own_leaf_nodes<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        LeafNode: traits::LeafNode<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId,
    ) -> Result<Vec<LeafNode>> {
        self.list_read(&Self::encode(group_id)?, tags::OWN_LEAF_NODES)
    }

    fn delete_own_leaf_nodes<GroupId: traits::GroupId<CURRENT_VERSION>>(
        &self, group_id: &GroupId,
    ) -> Result<()> {
        self.delete_by_tag(&Self::encode(group_id)?, tags::OWN_LEAF_NODES)
    }

    fn queue_proposal<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
        QueuedProposal: traits::QueuedProposal<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, proposal_ref: &ProposalRef, proposal: &QueuedProposal,
    ) -> Result<()> {
        let gid = Self::encode(group_id)?;
        let pref_bytes = Self::encode(proposal_ref)?;
        self.put(&gid, tags::QUEUED_PROPOSAL, &pref_bytes, Self::encode(proposal)?)?;
        self.list_append(&gid, tags::PROPOSAL_QUEUE_REFS, pref_bytes)
    }

    fn queued_proposal_refs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId,
    ) -> Result<Vec<ProposalRef>> {
        self.list_read(&Self::encode(group_id)?, tags::PROPOSAL_QUEUE_REFS)
    }

    fn queued_proposals<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
        QueuedProposal: traits::QueuedProposal<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId,
    ) -> Result<Vec<(ProposalRef, QueuedProposal)>> {
        let gid = Self::encode(group_id)?;
        // A ref's CBOR bytes are also the `sub_key` of its proposal row.
        let pref_bytes_list = self.list_raw(&gid, tags::PROPOSAL_QUEUE_REFS)?;
        let mut out = Vec::with_capacity(pref_bytes_list.len());
        for pref_bytes in pref_bytes_list {
            let pref: ProposalRef = Self::decode(&pref_bytes)?;
            let proposal =
                self.get(&gid, tags::QUEUED_PROPOSAL, &pref_bytes)?.ok_or_else(|| {
                    PromtuzMlsStorageError::decode("queued proposal missing for known ref")
                })?;
            out.push((pref, proposal));
        }
        Ok(out)
    }

    fn remove_proposal<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, proposal_ref: &ProposalRef,
    ) -> Result<()> {
        let gid = Self::encode(group_id)?;
        let pref_bytes = Self::encode(proposal_ref)?;
        self.delete_one(&gid, tags::QUEUED_PROPOSAL, &pref_bytes)?;
        self.list_remove(&gid, tags::PROPOSAL_QUEUE_REFS, &pref_bytes)
    }

    fn clear_proposal_queue<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        ProposalRef: traits::ProposalRef<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId,
    ) -> Result<()> {
        let gid = Self::encode(group_id)?;
        self.delete_by_tag(&gid, tags::QUEUED_PROPOSAL)?;
        self.delete_by_tag(&gid, tags::PROPOSAL_QUEUE_REFS)
    }

    fn write_encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, epoch: &EpochKey, leaf_index: u32, key_pairs: &[HpkeKeyPair],
    ) -> Result<()> {
        let gid = Self::encode(group_id)?;
        let sub_key = Self::encode(&(epoch, leaf_index))?;
        self.put(&gid, tags::EPOCH_KEY_PAIRS, &sub_key, Self::encode(&key_pairs)?)
    }

    fn encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
        HpkeKeyPair: traits::HpkeKeyPair<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, epoch: &EpochKey, leaf_index: u32,
    ) -> Result<Vec<HpkeKeyPair>> {
        let gid = Self::encode(group_id)?;
        let sub_key = Self::encode(&(epoch, leaf_index))?;
        Ok(self.get(&gid, tags::EPOCH_KEY_PAIRS, &sub_key)?.unwrap_or_default())
    }

    fn delete_encryption_epoch_key_pairs<
        GroupId: traits::GroupId<CURRENT_VERSION>,
        EpochKey: traits::EpochKey<CURRENT_VERSION>,
    >(
        &self, group_id: &GroupId, epoch: &EpochKey, leaf_index: u32,
    ) -> Result<()> {
        let gid = Self::encode(group_id)?;
        let sub_key = Self::encode(&(epoch, leaf_index))?;
        self.delete_one(&gid, tags::EPOCH_KEY_PAIRS, &sub_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::mls::with_failing_trigger;

    /// Through any mix of writes, budget refusals and failures on disk, a group's recorded size is
    /// the bytes it stores, and unscoped rows are never counted.
    #[test]
    fn group_size_is_the_bytes_stored_through_random_writes() {
        let conn = crate::db::Stores::in_memory(String::new()).mls();
        let p = PromtuzStorageProvider::new(conn.clone());
        let groups: [&[u8]; 3] = [b"one", b"two", b""];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        let big = MLS_GROUP_STATE_BUDGET_BYTES as usize / 2;
        let (mut refused, mut failed) = (0, 0);
        for round in 0..2000 {
            let group = groups[next(3) as usize];
            let (op, tag, sub, wiped) = (next(6), next(2) as i64, [next(3) as u8], next(4) as i64);
            let value = vec![7; if next(40) == 0 { big } else { 8 * next(6) as usize }];
            let fails = next(10) == 0;
            let write = || match op {
                0 | 1 => p.put(group, tag, &sub, value.clone()),
                2 => p.delete_one(group, tag, &sub),
                3 => p.list_append(group, 2 + tag, value.clone()),
                4 => p.list_remove(group, 2 + tag, &value),
                _ => p.delete_by_tag(group, wiped),
            };
            let result = if fails {
                with_failing_trigger(&conn, "INSERT ON mls_group_size", write)
            } else {
                write()
            };
            let over = matches!(result, Err(PromtuzMlsStorageError::BudgetExceeded { .. }));
            refused += usize::from(over);
            failed += usize::from(fails && result.is_err());
            let conn = conn.lock();
            for group in groups {
                let sum = |sql| conn.query_row(sql, [group], |r| r.get::<_, i64>(0)).unwrap();
                let stored = sum("SELECT COALESCE(SUM(length(value)), 0) FROM mls_storage \
                                  WHERE group_id = ?1");
                let counted = sum("SELECT COALESCE(SUM(total_bytes), 0) FROM mls_group_size \
                                   WHERE group_id = ?1");
                assert_eq!(counted, if group.is_empty() { 0 } else { stored }, "round {round}");
                assert!(group.is_empty() || stored as u64 <= MLS_GROUP_STATE_BUDGET_BYTES);
            }
        }
        assert!(refused > 0 && failed > 0, "refused {refused}, failed {failed}");
    }
}
