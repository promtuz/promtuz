//! Transactions and branch history for group commits.
//!
//! The MLS connection stays in one SQLite transaction from [`Transaction::open`] to
//! [`Transaction::publish`]: OpenMLS writes, branch history and outgoing work commit together, and
//! a failed or stale operation rolls back before reaching the network. Historical states only
//! process messages; nothing encrypts from an old snapshot or rewinds a sender ratchet.

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
use super::PromtuzStorageProvider;
use super::storage::Operation;

pub type Branch = [u8; 32];

pub fn operation_lock(group: &[u8; 32]) -> &'static Mutex<()> {
    let stripes = &crate::state::core().mls_operations;
    &stripes[usize::from(group[0]) % stripes.len()]
}

pub fn registered(provider: &PromtuzMlsProvider, gid: &[u8; 32]) -> Result<bool> {
    provider.storage().with_conn(|conn| {
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM mls_recovery_roots WHERE group_id=?1)",
            [gid],
            |r| r.get(0),
        )?)
    })
}

pub fn replay_watermark(provider: &PromtuzMlsProvider, gid: &[u8; 32]) -> Result<u64> {
    provider.storage().with_conn(|conn| {
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(sequence),0) FROM mls_replay WHERE group_id=?1",
            [gid],
            |r| r.get(0),
        )?)
    })
}

pub fn discard_replay(provider: &PromtuzMlsProvider, gid: &[u8; 32], id: &[u8; 16]) -> Result<()> {
    provider.storage().with_conn(|conn| {
        conn.execute("DELETE FROM mls_replay WHERE group_id=?1 AND dispatch_id=?2", params![gid, id])?;
        Ok(())
    })
}

pub fn clear_replay(provider: &PromtuzMlsProvider, gid: &[u8; 32], through: u64) -> Result<()> {
    provider.storage().with_conn(|conn| {
        conn.execute(
            "DELETE FROM mls_replay WHERE group_id=?1 AND sequence<=?2",
            params![gid, through],
        )?;
        Ok(())
    })
}

pub fn discard_message_replay(
    provider: &PromtuzMlsProvider, gid: &[u8; 32], target: &[u8; 16],
) -> Result<()> {
    use common::proto::mls_wire::AppPayload;
    use common::proto::pack::Unpacker;
    provider.storage().with_tx(|tx| {
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
        Ok(())
    })
}

pub fn recoverable_groups(provider: &PromtuzMlsProvider) -> Result<Vec<[u8; 32]>> {
    provider.storage().with_conn(|conn| {
        let mut q = conn.prepare("SELECT group_id FROM mls_recovery_roots r WHERE EXISTS(SELECT 1 FROM mls_branches b WHERE b.group_id=r.group_id AND b.parent IS NOT NULL) OR EXISTS(SELECT 1 FROM mls_join_history j WHERE j.group_id=r.group_id)")?;
        Ok(q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    })
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

/// A dispatch signed and framed for the durable outbox. Stored beside the epoch transition, it
/// closes the crash window between the MLS database and the outbox, separate SQLite files.
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

impl Received {
    /// Holds the plaintext for `groups::recovery::deliver_plaintext`. A group without branches
    /// stages under the zero branch.
    pub(crate) fn stage(
        &self, conn: &Connection, gid: &[u8; 32], branch: &Branch,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "INSERT OR IGNORE INTO mls_group_received(group_id,branch,sender,dispatch_id,accepted_at_ms,payload) VALUES(?1,?2,?3,?4,?5,?6)",
            params![gid, branch, self.author, self.id, self.accepted_at_ms, self.payload],
        )
    }
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
    operation:    Operation,
    key:          Vec<u8>,
    group_id:     [u8; 32],
    source:       Snapshot,
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

/// Builds a migration's replacement keys. The source stays until publish retires it, recording the
/// mapping and every invitation in the new session's transaction.
pub(super) struct Replacement {
    operation:    Operation,
    source:       [u8; 32],
    pub provider: PromtuzMlsProvider,
}

impl Replacement {
    pub fn open(provider: &PromtuzMlsProvider, source: [u8; 32]) -> Result<Self> {
        let operation = provider.storage().begin()?;
        ensure!(
            !Snapshot::read(operation.conn(), &storage_key(&source)?)?.0.is_empty(),
            "migration source is missing"
        );
        let provider = PromtuzMlsProvider::new(provider.storage().connection());
        Ok(Self { operation, source, provider })
    }

    /// Nothing may already stand where the replacement is built.
    pub fn ensure_absent(&self, target: &[u8; 32]) -> Result<()> {
        ensure!(
            Snapshot::read(self.operation.conn(), &storage_key(target)?)?.0.is_empty(),
            "migration target already exists"
        );
        Ok(())
    }

    pub fn publish(
        self, group: &MlsGroupHandle, history: &[GroupBranch], conversation: [u8; 16],
        jobs: &[DispatchJob], kp_ref: Option<[u8; 32]>,
    ) -> Result<()> {
        let gid = group.group_id();
        super::branch_proof::verify_history(&gid, history, group)?;
        let key = storage_key(&gid)?;
        let source_key = storage_key(&self.source)?;
        let Self { operation, source, .. } = self;
        let tx = operation.conn();
        let snapshot = Snapshot::read(tx, &key)?;
        tx.execute("INSERT INTO mls_group_migrations(group_id,target,conversation) VALUES(?1,?2,?3)",
            params![source,gid,conversation])?;
        let branch = group.branch_id();
        tx.execute("INSERT INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)", params![gid,branch])?;
        tx.execute("INSERT INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot,proof) VALUES(?1,?2,NULL,?3,0,X'',?4,?5)",
            params![gid,branch,group.epoch(),snapshot.encode()?,postcard::to_allocvec(history.last().expect("verified history"))?])?;
        tx.execute("INSERT INTO mls_join_history(group_id,inviter,history) VALUES(?1,?2,?3)",
            params![gid,history[0].author.0,postcard::to_allocvec(history)?])?;
        for job in jobs {
            tx.execute("INSERT INTO mls_dispatch_ids(group_id,dispatch_id,logical_id) VALUES(?1,?2,?3)",
                params![gid,job.id,job.logical_id])?;
            tx.execute("INSERT INTO mls_dispatch_jobs(group_id,branch,recipient,dispatch_id,kind,frame) VALUES(?1,?2,?3,?4,?5,?6)",
                params![gid,branch,job.recipient,job.id,job.kind,job.frame])?;
        }
        if let Some(kp_ref) = kp_ref {
            tx.execute("UPDATE mls_keypackage_stash SET consumed=1 WHERE kp_ref=?1", [kp_ref])?;
        }
        tx.execute("DELETE FROM mls_storage WHERE group_id=?1", [&source_key])?;
        tx.execute("DELETE FROM mls_group_size WHERE group_id=?1", [&source_key])?;
        tx.execute("DELETE FROM mls_migration_consents WHERE group_id=?1", [source])?;
        operation.commit()?;
        Ok(())
    }
}

impl Transaction {
    /// `None` selects the live head; a branch selects an arriving commit's retained parent, even
    /// when another commit already won locally. The connection stays locked in one transaction
    /// until [`Self::publish`], and a drop before that rolls everything back.
    pub fn open(
        provider: &PromtuzMlsProvider, group_id: [u8; 32], branch: Option<Branch>,
    ) -> Result<Option<Self>> {
        let operation = provider.storage().begin()?;
        let key = storage_key(&group_id)?;
        let live = Snapshot::read(operation.conn(), &key)?;
        if live.0.is_empty() {
            return Ok(None);
        }
        let retained = match branch {
            Some(id) => {
                let blob: Option<Vec<u8>> = operation
                    .conn()
                    .query_row(
                        "SELECT snapshot FROM mls_branches WHERE group_id=?1 AND branch=?2",
                        params![group_id, id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match blob {
                    Some(blob) if blob.is_empty() => return Ok(None),
                    Some(blob) => Some(Snapshot::decode(&blob)?),
                    None => None,
                }
            },
            None => None,
        };
        // A historical branch is processed on the live rows; publish puts the
        // canonical head back.
        let source = match retained {
            Some(rows) => {
                rows.write(operation.conn(), &key)?;
                rows
            },
            None => live,
        };
        let provider = PromtuzMlsProvider::new(provider.storage().connection());
        let group = MlsGroupHandle::load(&provider, &group_id)?
            .ok_or_else(|| anyhow!("group snapshot missing"))?;
        let parent = group.branch_id();
        if branch.is_some_and(|id| id != parent) {
            return Ok(None);
        }
        if branch.is_none() {
            let path = canonical_path(operation.conn(), &group_id)?;
            ensure!(
                path.last().is_none_or(|head| *head == parent),
                "group is recovering newer state"
            );
        }
        Ok(Some(Self { operation, key, group_id, source, parent, provider, group }))
    }

    /// Records the branch made, leaves the live rows at the canonical head and queues the outgoing
    /// work, all in the operation's transaction.
    pub fn publish(
        self, candidate: Option<Candidate>, jobs: &[DispatchJob], replay: Option<&Replay>,
        received: Option<&Received>,
    ) -> Result<Published> {
        let Self { operation, key, group_id, source, parent, provider: _, group } = self;
        let branch = group.branch_id();
        ensure!(candidate.is_some() || branch == parent, "unrecorded epoch transition");
        let tx = operation.conn();
        let snapshot = Snapshot::read(tx, &key)?;
        tx.execute(
            "INSERT OR IGNORE INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)",
            params![group_id, parent],
        )?;
        tx.execute("INSERT OR IGNORE INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot) VALUES(?1,?2,NULL,?3,0,X'',?4)",
            params![group_id, parent, group.epoch().saturating_sub(u64::from(candidate.is_some())), source.encode()?])?;
        let previous = canonical_path(tx, &group_id)?
            .last()
            .copied()
            .ok_or_else(|| anyhow!("group has no recovery root"))?;
        if let Some(candidate) = candidate {
            tx.execute("UPDATE mls_branches SET archived_at=COALESCE(archived_at,unixepoch()) WHERE group_id=?1 AND branch=?2",
                params![group_id,parent])?;
            let hash: [u8; 32] = Sha256::digest(&candidate.message).into();
            tx.execute("INSERT OR IGNORE INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,commit_blob,snapshot,change_blob,proof) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![group_id, branch, parent, group.epoch(), candidate.rank, hash,
                    candidate.message, snapshot.encode()?, candidate.change.map(|c| postcard::to_allocvec(&c)).transpose()?, candidate.proof.map(|p| postcard::to_allocvec(&p)).transpose()?])?;
        } else {
            tx.execute(
                "UPDATE mls_branches SET snapshot=?3 WHERE group_id=?1 AND branch=?2",
                params![group_id, branch, snapshot.encode()?],
            )?;
        }
        let path = canonical_path(tx, &group_id)?;
        let head = *path.last().ok_or_else(|| anyhow!("group has no canonical head"))?;
        let canonical = path.contains(&branch);
        let head_snapshot: Vec<u8> = tx.query_row(
            "SELECT snapshot FROM mls_branches WHERE group_id=?1 AND branch=?2",
            params![group_id, head],
            |r| r.get(0),
        )?;
        let needs_recovery = head_snapshot.is_empty();
        if !needs_recovery && head != branch {
            Snapshot::decode(&head_snapshot)?.write(tx, &key)?;
        }
        if let Some(replay) = replay {
            ensure!(branch == head, "cannot send from a historical branch");
            tx.execute("INSERT INTO mls_replay(group_id,dispatch_id,branch,payload,recipients,wake,kind) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(group_id,dispatch_id) DO UPDATE SET branch=excluded.branch",
                params![group_id, replay.id, branch, replay.payload, postcard::to_allocvec(&replay.recipients)?, replay.wake, replay.kind])?;
        }
        for job in jobs {
            tx.execute("INSERT OR IGNORE INTO mls_dispatch_ids(group_id,dispatch_id,logical_id) VALUES(?1,?2,?3)",
                params![group_id, job.id, job.logical_id])?;
            tx.execute("INSERT OR IGNORE INTO mls_dispatch_jobs(group_id,branch,recipient,dispatch_id,kind,frame) VALUES(?1,?2,?3,?4,?5,?6)",
                params![group_id, branch, job.recipient, job.id, job.kind, job.frame])?;
        }
        if let Some(received) = received {
            received.stage(tx, &group_id, &branch)?;
        }
        operation.commit()?;
        Ok(Published { previous, head, canonical, needs_recovery })
    }
}

pub fn logical_dispatch(id: &[u8]) -> Result<Vec<u8>> {
    let mapped: Option<Vec<u8>> = PromtuzMlsProvider::shared().storage().with_conn(|conn| {
        conn.query_row("SELECT logical_id FROM mls_dispatch_ids WHERE dispatch_id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
    })?;
    Ok(mapped.unwrap_or_else(|| id.to_vec()))
}

/// The branch-derived ids the copies of a group send went out under.
pub fn dispatch_ids(
    provider: &PromtuzMlsProvider, logical_id: &[u8],
) -> rusqlite::Result<Vec<Vec<u8>>> {
    provider.storage().with_conn(|conn| {
        let mut q = conn.prepare("SELECT dispatch_id FROM mls_dispatch_ids WHERE logical_id=?1")?;
        q.query_map([logical_id], |r| r.get(0))?.collect()
    })
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
    provider.storage().with_conn(|conn| history_in(conn, gid, parent))
}

fn history_in(conn: &Connection, gid: &[u8; 32], parent: Branch) -> Result<Vec<GroupBranch>> {
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
    super::branch_proof::verify_history(&gid, &[proof.clone()], group)?;
    provider.storage().with_tx(|tx| {
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM mls_recovery_roots WHERE group_id=?1)",
            [gid],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(());
        }
        let snapshot = Snapshot::read(tx, &storage_key(&gid)?)?;
        tx.execute(
            "INSERT INTO mls_recovery_roots(group_id,branch) VALUES(?1,?2)",
            params![gid, group.branch_id()],
        )?;
        tx.execute("INSERT INTO mls_branches(group_id,branch,parent,epoch,rank,commit_hash,snapshot,proof) VALUES(?1,?2,NULL,?3,0,?4,?5,?6)",
            params![gid,group.branch_id(),group.epoch(),proof.commit_hash.0,snapshot.encode()?,postcard::to_allocvec(proof)?])?;
        Ok(())
    })
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
    let operation = provider.storage().begin()?;
    let key = storage_key(&gid)?;
    // The history a replacement has to improve on, read before the Welcome
    // takes the rows over. A refused invitation rolls the old state back.
    let previous = if Snapshot::read(operation.conn(), &key)?.0.is_empty() {
        None
    } else {
        let old_group = MlsGroupHandle::load(provider, &gid)?
            .ok_or_else(|| anyhow!("old group missing"))?;
        let history = history_in(operation.conn(), &gid, old_group.branch_id())?;
        Snapshot(Vec::new()).write(operation.conn(), &key)?;
        Some(history)
    };
    let group = super::process_welcome(provider, envelope)?;
    let plaintext = super::branch_proof::open(
        provider,
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

    if let Some(previous) = previous {
        if restores_keys {
            ensure!(
                preferred_history(&previous, &history, true),
                "resync invitation moves onto a losing branch"
            );
        } else {
            let joined: Option<([u8; 32], Vec<u8>)> = operation
                .conn()
                .query_row(
                    "SELECT inviter,history FROM mls_join_history WHERE group_id=?1",
                    [gid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let (inviter, old) =
                joined.ok_or_else(|| anyhow!("existing member must request key recovery"))?;
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
    let tx = operation.conn();
    let snapshot = Snapshot::read(tx, &key)?;
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
    operation.commit()?;
    Ok(group)
}

/// Keep rollback secrets only for the transport's seven-day retry window.
/// Public signed history survives; a later fork uses an authenticated fresh
/// Welcome instead of silently reviving discarded key material.
pub fn prune(provider: &PromtuzMlsProvider, gid: &[u8; 32], now: u64) -> Result<()> {
    provider.storage().with_tx(|conn| {
        let Some(head) = canonical_path(conn, gid)?.last().copied() else { return Ok(()) };
        let cutoff = now.saturating_sub(7 * 24 * 60 * 60);
        conn.execute("UPDATE mls_branches SET snapshot=X'' WHERE group_id=?1 AND branch<>?2 AND COALESCE(archived_at,created_at)<?3",
            params![gid,head,cutoff])?;
        conn.execute(
            "UPDATE mls_group_received SET payload=X'' WHERE group_id=?1 AND applied=1",
            [gid],
        )?;
        conn.execute(
            "DELETE FROM mls_replay WHERE group_id=?1 AND created_at<?2",
            params![gid, cutoff],
        )?;
        conn.execute(
            "DELETE FROM mls_branch_inbox WHERE group_id=?1 AND received_at<?2",
            params![gid, cutoff],
        )?;
        Ok(())
    })
}

impl PromtuzStorageProvider {
    /// Erases every row of one group without loading it, so broken state goes too. The leaf
    /// signer, under the empty group id, survives.
    pub(crate) fn forget_group(&self, gid: &[u8; 32]) -> Result<()> {
        let key = storage_key(gid)?;
        self.with_tx(|tx| {
            tx.execute("DELETE FROM mls_storage WHERE group_id=?1", [&key])?;
            tx.execute("DELETE FROM mls_group_size WHERE group_id=?1", [&key])?;
            for table in ["mls_branches", "mls_recovery_roots", "mls_replay", "mls_dispatch_jobs",
                "mls_dispatch_ids", "mls_branch_inbox", "mls_group_received", "mls_join_history",
                "mls_recovery_retries", "mls_migration_consents", "mls_epoch_ahead"] {
                tx.execute(&format!("DELETE FROM {table} WHERE group_id=?1"), [gid])?;
            }
            tx.execute("DELETE FROM mls_group_migrations WHERE group_id=?1 OR target=?1", [gid])?;
            Ok(())
        })
    }
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
    provider.storage().with_conn(|conn| replay_needed_in(conn, group))
}

fn replay_needed_in(conn: &Connection, group: &[u8; 32]) -> Result<Vec<Replay>> {
    let path = canonical_path(conn, group)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// At each fork the higher rank wins and then the lower commit hash, whatever hangs below the
    /// losers; a cycle in stored history is an error rather than an endless walk.
    #[test]
    fn branch_election_follows_rank_then_commit_hash_and_refuses_a_cycle() {
        let conn = crate::db::Stores::in_memory(String::new()).mls();
        let conn = conn.lock();
        let (gid, branch) = ([7; 32], |n: u8| [n; 32]);
        conn.execute("INSERT INTO mls_recovery_roots VALUES (?1, ?2)", params![gid, branch(0)])
            .unwrap();
        // Branch, parent, rank and commit hash.
        let rows = [(0, None, 0, 0u8), (1, Some(0), 0, 1), (2, Some(0), 1, 9), (3, Some(2), 0, 5)];
        let rows = rows.into_iter().chain([(4, Some(2), 0, 3), (5, Some(1), 2, 0)]);
        for (id, parent, rank, hash) in rows {
            conn.execute(
                "INSERT INTO mls_branches(group_id, branch, parent, epoch, rank, commit_hash, \
                 snapshot) VALUES (?1, ?2, ?3, 0, ?4, ?5, X'')",
                params![gid, branch(id), parent.map(branch), rank, [hash; 32]],
            )
            .unwrap();
        }
        assert_eq!(canonical_path(&conn, &gid).unwrap(), [branch(0), branch(2), branch(4)]);

        conn.execute(
            "UPDATE mls_branches SET parent = ?2 WHERE group_id = ?1 AND branch = ?3",
            params![gid, branch(4), branch(0)],
        )
        .unwrap();
        assert!(canonical_path(&conn, &gid).is_err());
    }
}
