//! Holds MLS messages that arrive ahead of the group's epoch until a commit catches it up.

use std::sync::Arc;

use common::utils::now_ms;
use openmls::prelude::ContentType;
use openmls::prelude::ProcessedMessageContent;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::params;

use super::MAX_EPOCH_AHEAD_BUFFER;
use super::group::Changed;
use super::group::CommitOutcome;
use super::group::MlsGroupHandle;
use super::group::mls_message_from_bytes;
use super::provider::PromtuzMlsProvider;
use super::recovery::Received;
use super::types::MlsGroupError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Inserted,
    /// The buffer is full, so the incoming message was dropped.
    Discarded,
    /// Already buffered; the stored row is kept.
    Replaced,
}

/// Cap on rows one drain processes, so a huge backlog cannot stall the caller.
pub const EPOCH_CATCHUP_LIMIT: usize = 1024;

#[derive(Clone)]
pub struct EpochCatchupBuffer {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for EpochCatchupBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochCatchupBuffer").finish()
    }
}

/// What [`EpochCatchupBuffer::drain_when_ready`] produced, in order.
#[derive(Debug)]
pub enum Drained {
    Message(ProcessedApplicationMessage),
    /// A buffered commit merged and made this change.
    Change {
        changed:        Changed,
        accepted_at_ms: u64,
    },
}

#[derive(Debug)]
pub struct ProcessedApplicationMessage {
    /// The original dispatch id, or the buffer key of a row saved without one.
    pub dispatch_id: Vec<u8>,
    /// Verified outer sender when the original dispatch identity was saved.
    /// Distinct from the authenticated MLS author in `sender` below.
    pub dispatch_sender: Option<[u8; 32]>,
    pub plaintext: Vec<u8>,
    /// When the origin relay accepted the dispatch, so a buffered message keeps its send date.
    /// Rows from before that was recorded use their buffer time.
    pub accepted_at_ms: u64,
    /// The author, read off the authenticated MLS leaf at drain time rather than from whichever
    /// envelope unblocked the queue.
    pub sender: [u8; 32],
}

impl EpochCatchupBuffer {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// The caller has verified the dispatch signature over sender, id and payload.
    pub fn push_dispatch(
        &self, group: &MlsGroupHandle, msg_bytes: Vec<u8>, msg_epoch: u64,
        sender: [u8; 32], dispatch_id: [u8; 16], accepted_at_ms: u64,
    ) -> Result<PushOutcome, MlsGroupError> {
        // Keyed by sender and id together, so one member cannot take another's slot.
        let mut hash = blake3::Hasher::new();
        hash.update(b"promtuz-epoch-dispatch-v1\0");
        hash.update(&sender);
        hash.update(&dispatch_id);
        self.push_inner(
            group,
            msg_bytes,
            msg_epoch,
            hash.finalize().as_bytes().to_vec(),
            accepted_at_ms,
            Some((sender, dispatch_id)),
        )
    }

    fn push_inner(
        &self, group: &MlsGroupHandle, msg_bytes: Vec<u8>, msg_epoch: u64,
        dispatch_id: Vec<u8>, accepted_at_ms: u64,
        original: Option<([u8; 32], [u8; 16])>,
    ) -> Result<PushOutcome, MlsGroupError> {
        let group_id = group.group_id();
        let now_ms = now_ms();
        let mut conn = self.conn.lock();

        // One IMMEDIATE transaction, so two pushes cannot both pass the cap check.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM mls_epoch_ahead \
                 WHERE group_id = ?1 AND dispatch_id = ?2",
                params![&group_id[..], &dispatch_id],
                |r| r.get(0),
            )
            .ok();
        if existing.is_some() {
            tx.commit().map_err(|e| {
                MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e))
            })?;
            return Ok(PushOutcome::Replaced);
        }

        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM mls_epoch_ahead WHERE group_id = ?1",
                params![&group_id[..]],
                |r| r.get(0),
            )
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;

        let bytes: i64 = tx
            .query_row(
                "SELECT COALESCE(SUM(length(msg_blob)), 0) FROM mls_epoch_ahead \
                 WHERE group_id = ?1",
                params![&group_id[..]],
                |r| r.get(0),
            )
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;

        let over_rows = count as usize >= MAX_EPOCH_AHEAD_BUFFER;
        let over_bytes = bytes as u64 + msg_bytes.len() as u64 > super::MAX_EPOCH_AHEAD_BYTES;
        if over_rows || over_bytes {
            // Drop the newest, which is the incoming message.
            log::warn!(
                "EpochCatchupBuffer: group_id={} buffer full ({} rows, {} bytes), dropping newest \
                 (this group may be stuck — consider RequestRejoin)",
                hex::encode(&group_id[..4]),
                count,
                bytes
            );
            tx.commit().map_err(|e| {
                MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e))
            })?;
            return Ok(PushOutcome::Discarded);
        }

        tx.execute(
            "INSERT INTO mls_epoch_ahead \
             (group_id, epoch, dispatch_id, msg_blob, received_at_ms, accepted_at_ms, original_dispatch_id, dispatch_sender) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                &group_id[..],
                msg_epoch as i64,
                &dispatch_id,
                &msg_bytes,
                now_ms as i64,
                accepted_at_ms as i64,
                original.as_ref().map(|(_, id)| id.as_slice()),
                original.as_ref().map(|(sender, _)| sender.as_slice()),
            ],
        )
        .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
        tx.commit()
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
        Ok(PushOutcome::Inserted)
    }

    /// Merges buffered commits and stages buffered messages as the group's epoch reaches them. An
    /// undecryptable row is deleted; a failed write keeps its row and stops the drain.
    pub fn drain_when_ready(
        &self, group: &mut MlsGroupHandle, provider: &PromtuzMlsProvider,
    ) -> Vec<Drained> {
        let group_id = group.group_id();
        let mut output = Vec::new();
        let mut iterations = 0usize;

        let delete_row = |dispatch_id: &[u8]| match self.forget(provider, &group_id, dispatch_id) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("EpochCatchupBuffer: could not delete a drained row: {e}");
                false
            },
        };

        loop {
            let batch = self.ready(&group_id, group.epoch());
            if batch.is_empty() {
                break; // no progressable rows
            }
            for (dispatch_id, msg_blob, accepted_at_ms, original_id, dispatch_sender) in batch {
                if iterations >= EPOCH_CATCHUP_LIMIT {
                    log::warn!(
                        "EpochCatchupBuffer::drain_when_ready: group_id={} hit \
                         EPOCH_CATCHUP_LIMIT = {}, stopping drain (potentially stuck)",
                        hex::encode(&group_id[..4]),
                        EPOCH_CATCHUP_LIMIT
                    );
                    return output;
                }
                iterations += 1;

                let in_msg = match mls_message_from_bytes(&msg_blob) {
                    Ok(m) => m,
                    Err(e) => {
                        log::warn!("EpochCatchupBuffer: malformed buffered msg dropped: {e}");
                        if !delete_row(&dispatch_id) {
                            return output;
                        }
                        continue;
                    },
                };
                let proto = match in_msg.try_into_protocol_message() {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!(
                            "EpochCatchupBuffer: buffered msg is not a ProtocolMessage: {e:?}"
                        );
                        if !delete_row(&dispatch_id) {
                            return output;
                        }
                        continue;
                    },
                };

                // The message, what it stages or merges, and its row land together, so a failed
                // write leaves the group as it was and the row for the next drain.
                let landed = provider.storage().atomic(|| {
                    let processed =
                        group.process_incoming(provider, proto).map_err(Unlanded::Refused)?;
                    let sender = processed.sender;
                    let drained = match processed.content {
                        // The same gate as the live path: a commit that arrived early is no more
                        // trusted for having waited.
                        ProcessedMessageContent::StagedCommitMessage(staged) => {
                            let outcome = group
                                .merge_staged_commit_if_permitted(provider, *staged, sender)
                                .map_err(Unlanded::Failed)?;
                            match outcome {
                                CommitOutcome::Merged(Some(changed)) => {
                                    Some(Drained::Change { changed, accepted_at_ms })
                                },
                                _ => None,
                            }
                        },
                        ProcessedMessageContent::ApplicationMessage(app) => {
                            let message = ProcessedApplicationMessage {
                                dispatch_id: original_id.unwrap_or_else(|| dispatch_id.clone()),
                                dispatch_sender,
                                plaintext: app.into_bytes(),
                                accepted_at_ms,
                                sender,
                            };
                            let permitted =
                                group.application_is_permitted(&sender, &message.plaintext);
                            if permitted && let Ok(id) = message.dispatch_id.as_slice().try_into() {
                                let received = Received {
                                    author: sender,
                                    id,
                                    accepted_at_ms,
                                    payload: message.plaintext.clone(),
                                };
                                let staged = provider
                                    .storage()
                                    .with_conn(|conn| received.stage(conn, &group_id, &[0; 32]));
                                staged.map_err(|e| Unlanded::Failed(e.into()))?;
                            }
                            permitted.then_some(Drained::Message(message))
                        },
                        // Nothing uses a buffered proposal.
                        _ => None,
                    };
                    let forgotten = self.forget(provider, &group_id, &dispatch_id);
                    forgotten.map_err(|e| Unlanded::Failed(e.into()))?;
                    Ok(drained)
                });
                match landed {
                    Ok(drained) => output.extend(drained),
                    Err(Unlanded::Refused(e)) => {
                        log::warn!("EpochCatchupBuffer: dropping a buffered message: {e}");
                        if !delete_row(&dispatch_id) {
                            return output;
                        }
                    },
                    Err(Unlanded::Failed(e)) => {
                        log::warn!("EpochCatchupBuffer: buffered message kept for a retry: {e}");
                        // The handle took part of the change in memory; storage rolled back.
                        if let Ok(Some(at_epoch)) = MlsGroupHandle::load(provider, &group_id) {
                            *group = at_epoch;
                        }
                        return output;
                    },
                }
            }
        }

        output
    }

    /// Deletes a drained row, inside `provider`'s open operation when the buffer shares its
    /// database.
    fn forget(
        &self, provider: &PromtuzMlsProvider, group_id: &[u8; 32], dispatch_id: &[u8],
    ) -> rusqlite::Result<()> {
        let delete = |conn: &Connection| {
            let sql = "DELETE FROM mls_epoch_ahead WHERE group_id = ?1 AND dispatch_id = ?2";
            conn.execute(sql, params![&group_id[..], dispatch_id]).map(|_| ())
        };
        if Arc::ptr_eq(&self.conn, &provider.storage().connection()) {
            provider.storage().with_conn(delete)
        } else {
            delete(&self.conn.lock())
        }
    }

    /// The rows at the lowest buffered epoch the group has reached, in arrival order with its
    /// commits last: merging a commit retires the keys the epoch's other messages need.
    fn ready(&self, group_id: &[u8; 32], epoch: u64) -> Vec<Buffered> {
        let mut rows: Vec<Buffered> = crate::db::all(
            &self.conn.lock(),
            "SELECT dispatch_id, msg_blob, COALESCE(NULLIF(accepted_at_ms, 0), received_at_ms), \
                    original_dispatch_id, dispatch_sender \
             FROM mls_epoch_ahead \
             WHERE group_id = ?1 AND epoch = \
                (SELECT MIN(epoch) FROM mls_epoch_ahead WHERE group_id = ?1 AND epoch <= ?2) \
             ORDER BY received_at_ms ASC",
            params![&group_id[..], epoch as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? as u64, r.get(3)?, r.get(4)?)),
        )
        .unwrap_or_default();
        rows.sort_by_key(|row| is_commit(&row.1));
        rows
    }
}

/// Why a buffered message did not land. Either way its transaction rolled back.
enum Unlanded {
    /// It will never apply, so its row goes.
    Refused(MlsGroupError),
    /// Storage failed, so the row waits for the next drain.
    Failed(MlsGroupError),
}

impl From<super::types::PromtuzMlsStorageError> for Unlanded {
    fn from(e: super::types::PromtuzMlsStorageError) -> Self {
        Self::Failed(e.into())
    }
}

/// Dispatch id, message, acceptance time, original dispatch id and its sender.
type Buffered = (Vec<u8>, Vec<u8>, u64, Option<Vec<u8>>, Option<[u8; 32]>);

fn is_commit(msg_blob: &[u8]) -> bool {
    mls_message_from_bytes(msg_blob)
        .ok()
        .and_then(|m| m.try_into_protocol_message().ok())
        .is_some_and(|m| m.content_type() == ContentType::Commit)
}

#[cfg(test)]
mod tests {
    use openmls::prelude::MlsMessageOut;
    use openmls::prelude::tls_codec::Serialize as _;

    use super::*;
    use crate::mls::storage::tags;
    use crate::test_support::mls::*;

    fn bytes(msg: &MlsMessageOut) -> Vec<u8> {
        msg.tls_serialize_detached().unwrap()
    }

    fn messages(drained: Vec<Drained>) -> Vec<ProcessedApplicationMessage> {
        let message = |d| match d {
            Drained::Message(m) => Some(m),
            Drained::Change { .. } => None,
        };
        drained.into_iter().filter_map(message).collect()
    }

    fn buffered(conn: &Mutex<Connection>) -> i64 {
        conn.lock().query_row("SELECT COUNT(*) FROM mls_epoch_ahead", [], |r| r.get(0)).unwrap()
    }

    /// Buffered rows are keyed by outer sender and dispatch id, keep their first bytes and time,
    /// drain with the MLS author rather than the carrier, and drain only once.
    #[test]
    fn dispatch_identity_survives_reload_without_cross_sender_collisions() {
        let (alice, bob) = (Party::new(1), Party::new(2));
        let gid = [0xAA; 32];
        let (mut ga, [group]) = found(&alice, gid, None, [&bob]);
        let first = bytes(&alice.seal(&mut ga, b"first"));
        let forwarded = bytes(&alice.seal(&mut ga, b"forwarded"));
        let legacy = bytes(&alice.seal(&mut ga, b"legacy"));
        let buffer = EpochCatchupBuffer::new(bob.db.clone());
        let epoch = group.epoch();
        let push = |msg: &[u8], sender, at| {
            buffer.push_dispatch(&group, msg.to_vec(), epoch, sender, [9; 16], at).unwrap()
        };
        assert_eq!(push(&first, alice.ipk, 123), PushOutcome::Inserted);
        assert_eq!(push(&forwarded, alice.ipk, 999), PushOutcome::Replaced);
        assert_eq!(push(&forwarded, bob.ipk, 124), PushOutcome::Inserted);
        buffer.push_inner(&group, legacy, epoch, vec![10; 16], 125, None).unwrap();

        let buffer = EpochCatchupBuffer::new(bob.db.clone());
        let mut group = bob.group(&gid);
        let drained = messages(buffer.drain_when_ready(&mut group, &bob.provider));
        let row = |sender| {
            let m = drained.iter().find(|m| m.dispatch_sender == sender).unwrap();
            (m.dispatch_id.clone(), m.plaintext.clone(), m.accepted_at_ms, m.sender)
        };
        assert_eq!(drained.len(), 3);
        assert_eq!(row(Some(alice.ipk)), (vec![9; 16], b"first".to_vec(), 123, alice.ipk));
        assert_eq!(row(Some(bob.ipk)), (vec![9; 16], b"forwarded".to_vec(), 124, alice.ipk));
        assert_eq!(row(None), (vec![10; 16], b"legacy".to_vec(), 125, alice.ipk));
        assert_eq!(buffered(&bob.db), 0);

        buffer.push_dispatch(&group, first, epoch, alice.ipk, [9; 16], 123).unwrap();
        assert!(buffer.drain_when_ready(&mut group, &bob.provider).is_empty());
    }

    /// An epoch's messages drain before its commit whatever order they arrived in. A failed stage,
    /// delete or merge keeps the message or commit whole for the next drain.
    #[test]
    fn the_drain_never_loses_a_message_or_a_commit_to_arrival_order_or_a_failure() {
        let (alice, bob) = (Party::new(3), Party::new(4));
        let gid = [0x44; 32];
        let (mut ga, [mut gb]) = found(&alice, gid, None, [&bob]);
        let commit = |ga: &mut MlsGroupHandle| {
            let commit = alice.update(ga);
            ga.merge_pending_commit(&alice.provider).unwrap();
            commit
        };
        let to_two = commit(&mut ga);
        let x = alice.seal(&mut ga, b"x");
        let to_three = commit(&mut ga);
        let y = alice.seal(&mut ga, b"y");
        let buffer = EpochCatchupBuffer::new(bob.db.clone());
        // The commit that ends epoch 2 arrives before the post made in it.
        for (id, msg, epoch) in [(1, &to_three, 2), (2, &x, 2), (3, &y, 3)] {
            buffer.push_dispatch(&gb, bytes(msg), epoch, alice.ipk, [id; 16], 0).unwrap();
        }
        let staged = commit_of(bob.receive(&mut gb, &to_two));
        gb.merge_staged_commit(&bob.provider, staged).unwrap();

        let posts =
            |drained| messages(drained).into_iter().map(|m| m.plaintext).collect::<Vec<_>>();
        let (x, y) = (b"x".to_vec(), b"y".to_vec());
        // The epoch secrets are written after the new tree and context.
        let mid_merge =
            format!("INSERT ON mls_storage WHEN NEW.key_tag = {}", tags::GROUP_EPOCH_SECRETS);
        let failures = [
            ("INSERT ON mls_group_received", vec![], 3),
            ("DELETE ON mls_epoch_ahead", vec![], 3),
            (&mid_merge, vec![x.clone()], 2),
        ];
        for (failing, drained, left) in failures {
            let drain = || buffer.drain_when_ready(&mut gb, &bob.provider);
            let got = posts(with_failing_trigger(&bob.db, failing, drain));
            assert_eq!((got, buffered(&bob.db), gb.epoch()), (drained, left, 2), "{failing}");
        }
        assert_eq!(posts(buffer.drain_when_ready(&mut gb, &bob.provider)), [y.clone()]);
        assert_eq!(buffered(&bob.db), 0);
        assert_eq!(gb.epoch(), ga.epoch());
        let sql = "SELECT payload FROM mls_group_received ORDER BY rowid";
        let staged: Vec<Vec<u8>> = crate::db::all(&bob.db.lock(), sql, [], |r| r.get(0)).unwrap();
        assert_eq!(staged, [x, y]);
    }
}
