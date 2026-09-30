//! Transactions and branch history for group commits.
//!
//! OpenMLS makes several storage calls per operation. Run those calls against
//! an isolated provider, then publish the resulting group, its branch history,
//! and outgoing work in one SQLite transaction. Nothing from a failed or stale
//! operation may reach the network. Historical states are only used to process
//! messages; we never encrypt from an old snapshot or rewind a sender ratchet.

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use common::proto::mls_wire::GroupBranch;
use common::proto::mls_wire::SignedChange;
use common::proto::mls_wire::WelcomeEnvelopeP;
use openmls::prelude::GroupId;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::MlsGroupHandle;
use super::PromtuzMlsProvider;

pub type Branch = [u8; 32];

static OPERATIONS: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];

pub fn operation_lock(group: &[u8; 32]) -> &'static Mutex<()> {
    &OPERATIONS[usize::from(group[0]) % OPERATIONS.len()]
}

pub fn registered(provider: &PromtuzMlsProvider, gid: &[u8; 32]) -> Result<bool> {
    Ok(provider.storage().connection().lock().query_row(
        "SELECT EXISTS(SELECT 1 FROM mls_recovery_roots WHERE group_id=?1)",
        [gid],
        |r| r.get(0),
    )?)
}

pub fn replay_watermark(provider: &PromtuzMlsProvider, gid: &[u8; 32]) -> Result<u64> {
    Ok(provider.storage().connection().lock().query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM mls_replay WHERE group_id=?1",
        [gid],
        |r| r.get(0),
    )?)
}

pub fn discard_replay(provider: &PromtuzMlsProvider, gid: &[u8; 32], id: &[u8; 16]) -> Result<()> {
    provider
        .storage()
        .connection()
        .lock()
        .execute("DELETE FROM mls_replay WHERE group_id=?1 AND dispatch_id=?2", params![gid, id])?;
    Ok(())
}

pub fn clear_replay(provider: &PromtuzMlsProvider, gid: &[u8; 32], through: u64) -> Result<()> {
    provider.storage().connection().lock().execute(
        "DELETE FROM mls_replay WHERE group_id=?1 AND sequence<=?2",
        params![gid, through],
    )?;
    Ok(())
}

pub fn discard_message_replay(
    provider: &PromtuzMlsProvider, gid: &[u8; 32], target: &[u8; 16],
) -> Result<()> {
    use common::proto::mls_wire::AppPayload;
    use common::proto::pack::Unpacker;
    let connection = provider.storage().connection();
    let mut conn = connection.lock();
    let tx = conn.transaction()?;
    let rows = {
        let mut q = tx.prepare("SELECT dispatch_id,payload FROM mls_replay WHERE group_id=?1")?;
        q.query_map([gid], |r| Ok((r.get::<_, [u8; 16]>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (id, payload) in rows {
        let related = matches!(AppPayload::deser(&payload), Ok(AppPayload::Edit { target: t, .. }
            | AppPayload::Revise { target: t, .. } | AppPayload::React { target: t, .. }) if t == *target);
        if id == *target || related {
            tx.execute(
                "DELETE FROM mls_replay WHERE group_id=?1 AND dispatch_id=?2",
                params![gid, id],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn recoverable_groups(provider: &PromtuzMlsProvider) -> Result<Vec<[u8; 32]>> {
    let connection = provider.storage().connection();
    let conn = connection.lock();
    let mut q = conn.prepare("SELECT group_id FROM mls_recovery_roots r WHERE EXISTS(SELECT 1 FROM mls_branches b WHERE b.group_id=r.group_id AND b.parent IS NOT NULL) OR EXISTS(SELECT 1 FROM mls_join_history j WHERE j.group_id=r.group_id)")?;
    Ok(q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Row {
    tag:   i64,
    key:   Vec<u8>,
    value: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Snapshot(Vec<Row>);

fn storage_key(group: &[u8; 32]) -> Result<Vec<u8>> {
    let mut key = Vec::new();
    ciborium::ser::into_writer(&GroupId::from_slice(group), &mut key)?;
    Ok(key)
}

impl Snapshot {
    fn read(conn: &Connection, key: &[u8]) -> Result<Self> {
        let mut q = conn.prepare("SELECT key_tag, sub_key, value FROM mls_storage WHERE group_id=?1 ORDER BY key_tag, sub_key")?;
        Ok(Self(
            q.query_map([key], |r| {
                Ok(Row { tag: r.get(0)?, key: r.get(1)?, value: r.get(2)? })
            })?
            .collect::<rusqlite::Result<_>>()?,
        ))
    }

    fn write(&self, conn: &Connection, key: &[u8]) -> Result<()> {
        conn.execute("DELETE FROM mls_storage WHERE group_id=?1", [key])?;
        for row in &self.0 {
            conn.execute(
                "INSERT INTO mls_storage(group_id,key_tag,sub_key,value) VALUES(?1,?2,?3,?4)",
                params![key, row.tag, row.key, row.value],
            )?;
        }
        if !key.is_empty() {
            let bytes: usize = self.0.iter().map(|r| r.value.len()).sum();
            ensure!(
                bytes as u64 <= super::MLS_GROUP_STATE_BUDGET_BYTES,
                "group storage budget exceeded"
            );
            conn.execute(
                "INSERT OR REPLACE INTO mls_group_size(group_id,total_bytes) VALUES(?1,?2)",
                params![key, bytes as i64],
            )?;
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        Ok(lz4_flex::compress_prepend_size(&postcard::to_allocvec(self)?))
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        // Database contents, not an input received from another member.
        Ok(postcard::from_bytes(&lz4_flex::decompress_size_prepended(bytes)?)?)
    }
}

/// A dispatch already signed and framed, ready for the ordinary durable outbox.
/// Keeping it beside the epoch transition closes the crash window between the
/// MLS database and that outbox (which live in separate SQLite files).
pub struct DispatchJob {
    pub recipient:  [u8; 32],
    pub id:         [u8; 16],
    pub logical_id: [u8; 16],
    pub kind:       i64,
    pub frame:      Vec<u8>,
}

pub struct Replay {
    pub id:         [u8; 16],
    pub payload:    Vec<u8>,
    pub recipients: Vec<[u8; 32]>,
    pub wake:       i64,
    pub kind:       i64,
}

pub struct Received {
    pub author:         [u8; 32],
    pub id:             [u8; 16],
    pub accepted_at_ms: u64,
    pub payload:        Vec<u8>,
}

/// A verified commit's election key. Rank is read from its parent, never from
/// the roles that the commit grants itself. Timestamps play no part.
pub struct Candidate {
    pub rank:    u8,
    pub message: Vec<u8>,
    pub change:  Option<SignedChange>,
    pub proof:   Option<GroupBranch>,
}

pub struct Transaction {
    live:         Arc<Mutex<Connection>>,
    key:          Vec<u8>,
    group_id:     [u8; 32],
    expected:     Snapshot,
    source:       Snapshot,
    source_blob:  Option<Vec<u8>>,
    pub parent:   Branch,
    pub provider: PromtuzMlsProvider,
    pub group:    MlsGroupHandle,
}

pub struct Published {
    pub previous:       Branch,
    pub head:           Branch,
    pub canonical:      bool,
    pub needs_recovery: bool,
}

impl Transaction {
    /// `None` selects the live head. A branch selects the retained parent of an
    /// arriving commit, including when another commit has already won locally.
    pub fn open(
        provider: &PromtuzMlsProvider, group_id: [u8; 32], branch: Option<Branch>,
    ) -> Result<Option<Self>> {
        let live = provider.storage().connection();
        let key = storage_key(&group_id)?;
        let (expected, globals, source_blob) = {
            let conn = live.lock();
            let expected = Snapshot::read(&conn, &key)?;
            if expected.0.is_empty() {
                return Ok(None);
            }
            let blob = branch
                .map(|id| {
                    conn.query_row(
                        "SELECT snapshot FROM mls_branches WHERE group_id=?1 AND branch=?2",
                        params![group_id, id],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .optional()
                })
                .transpose()?
                .flatten();
            (expected, Snapshot::read(&conn, &[])?, blob)
        };
        if source_blob.as_ref().is_some_and(|b| b.is_empty()) {
            return Ok(None);
        }
        let source = source_blob
            .as_deref()
            .map(Snapshot::decode)
            .transpose()?
            .unwrap_or_else(|| expected.clone());
        let mut isolated = Connection::open_in_memory()?;
        crate::db::mls::apply_mls_migrations(&mut isolated);
        globals.write(&isolated, &[])?;
        source.write(&isolated, &key)?;
        let fork = PromtuzMlsProvider::new(Arc::new(Mutex::new(isolated)));
        let group = MlsGroupHandle::load(&fork, &group_id)?
            .ok_or_else(|| anyhow!("group snapshot missing"))?;
        let parent = group.branch_id();
        if branch.is_some_and(|id| id != parent) {
            return Ok(None);
        }
        if branch.is_none() {
            let path = canonical_path(&live.lock(), &group_id)?;
            ensure!(
                path.last().is_none_or(|head| *head == parent),
                "group is recovering newer state"
            );
        }
        Ok(Some(Self {
            live,
            key,
            group_id,
            expected,
            source,
            source_blob,
            parent,
            provider: fork,
            group,
        }))
    }

    /// Atomically publish the result. A concurrent operation invalidates the
    /// snapshot comparison; its caller retries from fresh state, never sends
    /// the ciphertext produced by the rejected transaction.
    pub fn publish(
        self, candidate: Option<Candidate>, jobs: &[DispatchJob], replay: Option<&Replay>,
        received: Option<&Received>,
    ) -> Result<Published> {
        let branch = self.group.branch_id();
        ensure!(candidate.is_some() || branch == self.parent, "unrecorded epoch transition");
        let snapshot = Snapshot::read(&self.provider.storage().connection().lock(), &self.key)?;
        let mut conn = self.live.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(
            Snapshot::read(&tx, &self.key)? == self.expected,
            "group changed concurrently; retry"
        );
        if let Some(expected) = self.source_blob.as_ref() {
            let actual: Vec<u8> = tx.query_row(
                "SELECT snapshot FROM mls_branches WHERE group_id=?1 AND branch=?2",
                params![self.group_id, self.parent],
                |r| r.get(0),
            )?;
            ensure!(&actual == expected, "historical group changed concurrently; retry");
        }
        tx.execute(
            "INSERT OR IGNORE INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)",
            params![self.group_id, self.parent],
        )?;
        tx.execute("INSERT OR IGNORE INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot) VALUES(?1,?2,NULL,?3,0,X'',?4)",
            params![self.group_id, self.parent, self.group.epoch().saturating_sub(u64::from(candidate.is_some())), self.source.encode()?])?;
        let previous = canonical_path(&tx, &self.group_id)?
            .last()
            .copied()
            .ok_or_else(|| anyhow!("group has no recovery root"))?;
        if let Some(candidate) = candidate {
            tx.execute("UPDATE mls_branches SET archived_at=COALESCE(archived_at,unixepoch()) WHERE group_id=?1 AND branch=?2",
                params![self.group_id,self.parent])?;
            let hash: [u8; 32] = Sha256::digest(&candidate.message).into();
            tx.execute("INSERT OR IGNORE INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,commit_blob,snapshot,change_blob,proof) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![self.group_id, branch, self.parent, self.group.epoch(), candidate.rank, hash,
                    candidate.message, snapshot.encode()?, candidate.change.map(|c| postcard::to_allocvec(&c)).transpose()?, candidate.proof.map(|p| postcard::to_allocvec(&p)).transpose()?])?;
        } else {
            tx.execute(
                "UPDATE mls_branches SET snapshot=?3 WHERE group_id=?1 AND branch=?2",
                params![self.group_id, branch, snapshot.encode()?],
            )?;
        }
        let path = canonical_path(&tx, &self.group_id)?;
        let head = *path.last().ok_or_else(|| anyhow!("group has no canonical head"))?;
        let canonical = path.contains(&branch);
        let head_snapshot: Vec<u8> = tx.query_row(
            "SELECT snapshot FROM mls_branches WHERE group_id=?1 AND branch=?2",
            params![self.group_id, head],
            |r| r.get(0),
        )?;
        let needs_recovery = head_snapshot.is_empty();
        if !needs_recovery {
            Snapshot::decode(&head_snapshot)?.write(&tx, &self.key)?;
        }
        if let Some(replay) = replay {
            ensure!(branch == head, "cannot send from a historical branch");
            tx.execute("INSERT INTO mls_replay(group_id,dispatch_id,branch,payload,recipients,wake,kind) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(group_id,dispatch_id) DO UPDATE SET branch=excluded.branch",
                params![self.group_id, replay.id, branch, replay.payload, postcard::to_allocvec(&replay.recipients)?, replay.wake, replay.kind])?;
        }
        for job in jobs {
            tx.execute("INSERT OR IGNORE INTO mls_dispatch_ids(group_id,dispatch_id,logical_id) VALUES(?1,?2,?3)",
                params![self.group_id, job.id, job.logical_id])?;
            tx.execute("INSERT OR IGNORE INTO mls_dispatch_jobs(group_id,branch,recipient,dispatch_id,kind,frame) VALUES(?1,?2,?3,?4,?5,?6)",
                params![self.group_id, branch, job.recipient, job.id, job.kind, job.frame])?;
        }
        if let Some(received) = received {
            tx.execute("INSERT OR IGNORE INTO mls_group_received(group_id,branch,sender,dispatch_id,accepted_at_ms,payload) VALUES(?1,?2,?3,?4,?5,?6)",
                params![self.group_id, branch, received.author, received.id, received.accepted_at_ms, received.payload])?;
        }
        tx.commit()?;
        Ok(Published { previous, head, canonical, needs_recovery })
    }
}

pub fn logical_dispatch(id: &[u8]) -> Result<Vec<u8>> {
    let conn = crate::db::mls::stash_db_handle();
    let mapped: Option<Vec<u8>> = conn
        .lock()
        .query_row("SELECT logical_id FROM mls_dispatch_ids WHERE dispatch_id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(mapped.unwrap_or_else(|| id.to_vec()))
}

pub fn dispatch_id(branch: &Branch, logical_id: &[u8; 16]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update(b"promtuz group dispatch v1");
    hash.update(branch);
    hash.update(logical_id);
    hash.finalize()[..16].try_into().expect("16-byte hash prefix")
}

pub fn history(
    provider: &PromtuzMlsProvider, gid: &[u8; 32], parent: Branch,
) -> Result<Vec<GroupBranch>> {
    let connection = provider.storage().connection();
    let conn = connection.lock();
    let prefix: Option<Vec<u8>> = conn
        .query_row("SELECT history FROM mls_join_history WHERE group_id=?1", [gid], |r| r.get(0))
        .optional()?;
    let mut history: Vec<GroupBranch> =
        prefix.as_deref().map(postcard::from_bytes).transpose()?.unwrap_or_default();
    let mut ancestry = Vec::new();
    let mut branch = Some(parent);
    while let Some(current) = branch {
        ensure!(!ancestry.contains(&current), "cyclic group history");
        ancestry.push(current);
        branch = conn
            .query_row(
                "SELECT parent FROM mls_branches WHERE group_id=?1 AND branch=?2",
                params![gid, current],
                |r| r.get::<_, Option<[u8; 32]>>(0),
            )
            .optional()?
            .flatten();
    }
    ancestry.reverse();
    for branch in ancestry {
        if history.last().is_some_and(|h| h.branch.0 == branch) {
            continue;
        }
        let proof: Vec<u8> = conn.query_row(
            "SELECT proof FROM mls_branches WHERE group_id=?1 AND branch=?2",
            params![gid, branch],
            |r| r.get(0),
        )?;
        history.push(postcard::from_bytes(&proof)?);
    }
    ensure!(
        !history.is_empty() && history.last().is_some_and(|p| p.branch.0 == parent),
        "signed group history is missing"
    );
    Ok(history)
}

pub fn ensure_root(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, signer: &ed25519_dalek::SigningKey,
) -> Result<()> {
    if registered(provider, &group.group_id())? {
        return Ok(());
    }
    let proof = super::branch_proof::sign(group, None, 0, &[], signer)?;
    accept_root(provider, group, &proof)
}

pub fn accept_root(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, proof: &GroupBranch,
) -> Result<()> {
    let gid = group.group_id();
    let connection = provider.storage().connection();
    let mut conn = connection.lock();
    if conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM mls_recovery_roots WHERE group_id=?1)",
        [gid],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(());
    }
    super::branch_proof::verify_history(&gid, &[proof.clone()], group)?;
    let snapshot = Snapshot::read(&conn, &storage_key(&gid)?)?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)",
        params![gid, group.branch_id()],
    )?;
    tx.execute("INSERT INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot,proof) VALUES(?1,?2,NULL,?3,0,?4,?5,?6)",
        params![gid,group.branch_id(),group.epoch(),proof.commit_hash.0,snapshot.encode()?,postcard::to_allocvec(proof)?])?;
    tx.commit()?;
    Ok(())
}

pub fn next_history(
    provider: &PromtuzMlsProvider, gid: &[u8; 32], parent: Branch, proof: &GroupBranch,
) -> Result<Vec<GroupBranch>> {
    let mut path = history(provider, gid, parent)?;
    path.push(proof.clone());
    Ok(path)
}

/// Replacement welcomes only repair divergence preceding this device's join.
/// Existing members resolve their own branches cryptographically; they never
/// accept an inviter's assertion in place of the history they already hold.
fn preferred_history(old: &[GroupBranch], new: &[GroupBranch], allow_extension: bool) -> bool {
    if old.first() != new.first() {
        return false;
    }
    for (old, new) in old.iter().zip(new) {
        if old.branch != new.branch {
            return new.rank > old.rank
                || (new.rank == old.rank && new.commit_hash.0 < old.commit_hash.0);
        }
        if old.rank != new.rank || old.commit_hash != new.commit_hash {
            return false;
        }
    }
    allow_extension && new.len() > old.len()
}

pub fn accept_welcome(
    provider: &PromtuzMlsProvider, envelope: &WelcomeEnvelopeP, sealed_history: &[u8],
    refresh: Option<&common::proto::mls_wire::GroupMemberRequest>, anchor: Option<Branch>,
) -> Result<MlsGroupHandle> {
    let gid = envelope.group_id.0;
    let _operation = operation_lock(&gid).lock();
    let live = provider.storage().connection();
    let key = storage_key(&gid)?;
    let (expected, globals) = {
        let conn = live.lock();
        let expected = Snapshot::read(&conn, &key)?;
        (expected, Snapshot::read(&conn, &[])?)
    };
    let mut isolated = Connection::open_in_memory()?;
    crate::db::mls::apply_mls_migrations(&mut isolated);
    globals.write(&isolated, &[])?;
    let fork = PromtuzMlsProvider::new(Arc::new(Mutex::new(isolated)));
    let group = super::process_welcome(&fork, envelope)?;
    let plaintext = super::branch_proof::open(
        &fork,
        &group,
        super::branch_proof::INVITATION_LABEL,
        sealed_history,
    )?;
    let history: Vec<GroupBranch> = postcard::from_bytes(&plaintext)?;
    super::branch_proof::verify_history(&gid, &history, &group)?;
    if let Some(anchor) = anchor {
        ensure!(history[0].branch.0 == anchor, "invitation changes group identity");
    }
    let restores_keys = refresh.is_some_and(|request| {
        group.group_meta().and_then(|m| m.state).and_then(|s| s.last).is_some_and(|c| {
            c.change == common::proto::mls_wire::GroupChange::MemberRequest(request.clone())
        })
    });

    if !expected.0.is_empty() {
        if restores_keys {
            let old_group = MlsGroupHandle::load(provider, &gid)?
                .ok_or_else(|| anyhow!("old group missing"))?;
            let previous = self::history(provider, &gid, old_group.branch_id())?;
            ensure!(
                preferred_history(&previous, &history, true),
                "resync invitation moves onto a losing branch"
            );
        } else {
            let conn = live.lock();
            let previous: Option<([u8; 32], Vec<u8>)> = conn
                .query_row(
                    "SELECT inviter,history FROM mls_join_history WHERE group_id=?1",
                    [gid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (inviter, old) =
                previous.ok_or_else(|| anyhow!("existing member must request key recovery"))?;
            ensure!(
                inviter == envelope.sender_ipk.0,
                "replacement invitation is not from the original inviter"
            );
            ensure!(
                preferred_history(
                    &postcard::from_bytes::<Vec<GroupBranch>>(&old)?,
                    &history,
                    false
                ),
                "invitation does not repair a losing branch"
            );
        }
    } else if refresh.is_some() {
        ensure!(
            restores_keys && anchor.is_some(),
            "key recovery has no matching request or identity anchor"
        );
    }
    ensure!(
        group.group_meta().is_some_and(|m| m.state.is_some()),
        "invitation is not a group with signed rules"
    );
    ensure!(
        history.last().is_some_and(|p| p.branch.0 == group.branch_id()),
        "invitation history does not identify its MLS state"
    );
    let fresh = fork.storage().connection();
    let fresh = fresh.lock();
    let snapshot = Snapshot::read(&fresh, &key)?;
    let updated_globals = Snapshot::read(&fresh, &[])?;
    let mut conn = live.lock();
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(Snapshot::read(&tx, &key)? == expected, "group changed while processing an invitation");
    // Apply only the global keys this Welcome consumed or created. A concurrent
    // stash refill and unrelated groups' signing keys must survive.
    for old in &globals.0 {
        if updated_globals
            .0
            .iter()
            .any(|r| r.tag == old.tag && r.key == old.key && r.value == old.value)
        {
            continue;
        }
        let current: Option<Vec<u8>> = tx
            .query_row(
                "SELECT value FROM mls_storage WHERE group_id=X'' AND key_tag=?1 AND sub_key=?2",
                params![old.tag, old.key],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(current.as_ref() == Some(&old.value), "invitation key was consumed concurrently");
        tx.execute(
            "DELETE FROM mls_storage WHERE group_id=X'' AND key_tag=?1 AND sub_key=?2",
            params![old.tag, old.key],
        )?;
    }
    for row in &updated_globals.0 {
        if globals.0.contains(row) {
            continue;
        }
        tx.execute(
            "INSERT INTO mls_storage(group_id,key_tag,sub_key,value) VALUES(X'',?1,?2,?3)",
            params![row.tag, row.key, row.value],
        )?;
    }
    snapshot.write(&tx, &key)?;
    let branch = group.branch_id();
    tx.execute(
        "INSERT OR REPLACE INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)",
        params![gid, branch],
    )?;
    tx.execute("INSERT OR IGNORE INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot,proof) VALUES(?1,?2,NULL,?3,0,X'',?4,?5)",
        params![gid,branch,group.epoch(),snapshot.encode()?,postcard::to_allocvec(history.last().expect("validated history"))?])?;
    tx.execute(
        "INSERT OR REPLACE INTO mls_join_history(group_id,inviter,history) VALUES(?1,?2,?3)",
        params![gid, envelope.sender_ipk.0, postcard::to_allocvec(&history)?],
    )?;
    tx.execute(
        "UPDATE mls_keypackage_stash SET consumed=1 WHERE kp_ref=?1",
        [envelope.kp_ref_used.0],
    )?;
    tx.commit()?;
    drop(conn);
    MlsGroupHandle::load(provider, &gid)?.ok_or_else(|| anyhow!("invited group missing"))
}

/// Keep rollback secrets only for the transport's seven-day retry window.
/// Public signed history survives; a later fork uses an authenticated fresh
/// Welcome instead of silently reviving discarded key material.
pub fn prune(provider: &PromtuzMlsProvider, gid: &[u8; 32], now: u64) -> Result<()> {
    let connection = provider.storage().connection();
    let conn = connection.lock();
    let Some(head) = canonical_path(&conn, gid)?.last().copied() else { return Ok(()) };
    conn.execute("UPDATE mls_branches SET snapshot=X'' WHERE group_id=?1 AND branch<>?2 AND COALESCE(archived_at,created_at)<?3",
        params![gid,head,now.saturating_sub(7 * 24 * 60 * 60)])?;
    conn.execute(
        "UPDATE mls_group_received SET payload=X'' WHERE group_id=?1 AND applied=1",
        [gid],
    )?;
    let cutoff = now.saturating_sub(7 * 24 * 60 * 60);
    conn.execute(
        "DELETE FROM mls_replay WHERE group_id=?1 AND created_at<?2",
        params![gid, cutoff],
    )?;
    conn.execute(
        "DELETE FROM mls_branch_inbox WHERE group_id=?1 AND received_at<?2",
        params![gid, cutoff],
    )?;
    Ok(())
}

pub fn canonical_path(conn: &Connection, group: &[u8; 32]) -> Result<Vec<Branch>> {
    let root: Option<Vec<u8>> = conn
        .query_row("SELECT branch FROM mls_recovery_roots WHERE group_id=?1", [group], |r| r.get(0))
        .optional()?;
    let Some(root) = root else { return Ok(Vec::new()) };
    let mut path = vec![root.try_into().map_err(|_| anyhow!("invalid branch id"))?];
    loop {
        let next: Option<Vec<u8>> = conn.query_row(
            "SELECT branch FROM mls_branches WHERE group_id=?1 AND parent=?2 ORDER BY rank DESC, commit_hash ASC LIMIT 1",
            params![group, path.last()], |r| r.get(0)).optional()?;
        let Some(next) = next else { return Ok(path) };
        let next = next.try_into().map_err(|_| anyhow!("invalid branch id"))?;
        if path.contains(&next) {
            bail!("cyclic group history");
        }
        path.push(next);
    }
}

pub fn replay_needed(provider: &PromtuzMlsProvider, group: &[u8; 32]) -> Result<Vec<Replay>> {
    let conn = provider.storage().connection();
    let conn = conn.lock();
    let path = canonical_path(&conn, group)?;
    let mut q = conn.prepare("SELECT dispatch_id,branch,payload,recipients,wake,kind FROM mls_replay WHERE group_id=?1 ORDER BY rowid")?;
    let mut rows = q.query([group])?;
    let mut pending = Vec::new();
    while let Some(row) = rows.next()? {
        let branch: Vec<u8> = row.get(1)?;
        if path.iter().any(|b| b.as_slice() == branch) {
            continue;
        }
        pending.push(Replay {
            id:         row
                .get::<_, Vec<u8>>(0)?
                .try_into()
                .map_err(|_| anyhow!("invalid dispatch id"))?,
            payload:    row.get(2)?,
            recipients: postcard::from_bytes(&row.get::<_, Vec<u8>>(3)?)?,
            wake:       row.get(4)?,
            kind:       row.get(5)?,
        });
    }
    Ok(pending)
}
