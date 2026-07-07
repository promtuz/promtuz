//! Out-of-order epoch buffer.
//!
//! # Problem
//!
//! MLS is *strictly epoch-ordered*: a Commit `C_n` advances the group
//! from epoch `n-1` to epoch `n`. Application messages are tagged
//! with their epoch. Because promtuz's home relays may deliver
//! out-of-order — or because a recipient drains a multi-week backlog
//! after reconnect — the recipient frequently sees an Application
//! at epoch=N+k *before* the load-bearing Commit that advances them
//! from N → N+1 → ... → N+k.
//!
//! Within a single epoch, openmls handles reorder natively (skip-key
//! cache). **Across** epochs, we (promtuz) must buffer. That's this
//! module.
//!
//! # Storage
//!
//! Backed by a SQLite table `mls_epoch_ahead` (migration v2):
//!
//! ```sql
//! CREATE TABLE mls_epoch_ahead (
//!     group_id        BLOB    NOT NULL,
//!     epoch           INTEGER NOT NULL,
//!     dispatch_id     BLOB    NOT NULL,
//!     msg_blob        BLOB    NOT NULL,
//!     received_at_ms  INTEGER NOT NULL,
//!     PRIMARY KEY (group_id, dispatch_id)
//! );
//! ```
//!
//! `dispatch_id` is the outer `DispatchP::id` (UUIDv7) bytes — keeping
//! the dedup boundary at the dispatch layer matches the rest of
//! promtuz's idempotency discipline.
//!
//! # Bounded buffer
//!
//! Cap is `MAX_EPOCH_AHEAD_BUFFER = 512` per group.
//! On overflow we **drop the newest entry** (stack-style):
//!
//! > Reasoning: the oldest entries are most likely the load-bearing
//! > commits, and we want to preserve those because a missing commit
//! > blocks epoch advance. New application messages at a far-ahead
//! > epoch are more likely to also be load-bearing-supplemented;
//! > dropping them is preferable to dropping commits.
//!
//! "Newest" is defined by `received_at_ms` (the wall-clock time at
//! `push`); when a new push would exceed the cap, the *incoming*
//! message itself is dropped (because all existing rows have an
//! earlier `received_at_ms` than `now`). [`PushOutcome`] surfaces
//! the result so callers can log / surface a UI signal.
//!
//! # Drain
//!
//! After processing a commit that advances the local epoch, the
//! caller invokes [`EpochCatchupBuffer::drain_when_ready`] which:
//!
//! 1. Re-scans the table for rows where `epoch <= group.epoch()`.
//! 2. Feeds each row's `msg_blob` to the group's
//!    `process_incoming` — yielding either a decrypted Application,
//!    a staged Commit (which advances epoch further), or an error
//!    (msg too old / cipher invalid).
//! 3. Applications are returned to the caller; staged commits are
//!    auto-merged in the same loop iteration.
//! 4. Repeats until no more progress.
//!
//! The drain is **bounded** — at most `EPOCH_CATCHUP_LIMIT = 1024`
//! commits per call. Beyond that, we just stop draining.

// Public surface here is consumed by `messaging.rs`; the cdylib
// compiler can't see across the JNI boundary so flags it as dead.
// Mirrors the `provider.rs` pattern.
#![allow(dead_code)]

use std::sync::Arc;

use openmls::prelude::ApplicationMessage;
use openmls::prelude::ProcessedMessageContent;
use parking_lot::Mutex;
use rusqlite::params;
use rusqlite::Connection;

use super::group::mls_message_from_bytes;
use super::group::MlsGroupHandle;
use super::provider::PromtuzMlsProvider;
use super::types::MlsGroupError;
use super::MAX_EPOCH_AHEAD_BUFFER;

/// Outcome of [`EpochCatchupBuffer::push`]. Mirrors the
/// task-prompt's contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // messaging.rs caller.
pub enum PushOutcome {
    /// Row was inserted. Buffer count for this group did not exceed
    /// the cap.
    Inserted,
    /// The incoming row was dropped because the buffer is at cap.
    /// We drop the *newest* — and since the incoming is by definition
    /// newest, it doesn't get persisted. The caller should log the
    /// discard.
    Discarded,
    /// A row with the same `(group_id, dispatch_id)` already exists.
    /// We treat this as idempotent; the existing row's `msg_blob` is
    /// preserved (the duplicate is bytewise identical anyway by
    /// dispatch-layer deduplication).
    Replaced,
}

/// Hard cap on commits processed per [`EpochCatchupBuffer::drain_when_ready`]
/// call. Protects against pathological backlogs that would block the
/// caller for >1s.
#[allow(dead_code)] // messaging.rs caller.
pub const EPOCH_CATCHUP_LIMIT: usize = 1024;

/// Out-of-order epoch buffer for a single libcore.
///
/// **Single instance per libcore process** — the buffer is
/// `(group_id, dispatch_id)`-keyed across *all* groups, so we share
/// one instance. Cloneable via the inner `Arc<Mutex<Connection>>`.
#[derive(Clone)]
#[allow(dead_code)] // messaging.rs caller.
pub struct EpochCatchupBuffer {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for EpochCatchupBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpochCatchupBuffer").finish()
    }
}

/// Decrypted application message returned by
/// [`EpochCatchupBuffer::drain_when_ready`].
#[derive(Debug)]
#[allow(dead_code)] // messaging.rs caller.
pub struct ProcessedApplicationMessage {
    /// The dispatch id of the buffered envelope. Caller may use this
    /// to ack the corresponding row in `cf_dht_queue`.
    pub dispatch_id: Vec<u8>,
    /// Plaintext bytes the application wrote at send time.
    pub plaintext: Vec<u8>,
    /// Epoch at which the message was encrypted. Always `<=`
    /// `group.epoch()` after a successful drain step.
    pub epoch: u64,
}

#[allow(dead_code)] // messaging.rs caller.
impl EpochCatchupBuffer {
    /// Build a buffer over a caller-supplied SQLite connection.
    /// The connection must already have the MLS migrations applied
    /// (which include the `mls_epoch_ahead` table).
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Push a buffered MLS message for a group.
    ///
    /// `msg_epoch` is the message's claimed epoch (parsed from the
    /// `MlsApplicationEnvelopeP::epoch` field by the caller — we do
    /// not re-parse here because the caller has the postcard
    /// envelope already). `dispatch_id` is the outer DispatchP id
    /// bytes.
    ///
    /// If `msg_epoch < group.epoch()`, the caller should *feed directly*
    /// to the group (openmls may decrypt from cached past-epoch material)
    /// rather than buffering. We don't enforce this here; the buffer is
    /// happy to stash any epoch — but in practice the caller filters
    /// before pushing.
    ///
    /// Returns:
    /// - [`PushOutcome::Inserted`] on a fresh row,
    /// - [`PushOutcome::Replaced`] on a `(group_id, dispatch_id)`
    ///   duplicate (idempotent),
    /// - [`PushOutcome::Discarded`] if the group's buffer count
    ///   already equals [`MAX_EPOCH_AHEAD_BUFFER`].
    pub fn push(
        &self, group: &MlsGroupHandle, msg_bytes: Vec<u8>, msg_epoch: u64,
        dispatch_id: Vec<u8>,
    ) -> Result<PushOutcome, MlsGroupError> {
        let group_id = group.group_id();
        let now_ms = unix_now_ms();
        let mut conn = self.conn.lock();

        // Wrap the count-then-insert in a single BEGIN IMMEDIATE
        // transaction so two concurrent pushes can't both observe
        // `count = MAX-1` and both insert (silently bumping the buffer
        // above the cap).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;

        // Idempotent on (group_id, dispatch_id).
        let existing: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM mls_epoch_ahead \
                 WHERE group_id = ?1 AND dispatch_id = ?2",
                params![&group_id[..], &dispatch_id],
                |r| r.get(0),
            )
            .ok();
        if existing.is_some() {
            tx.commit()
                .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
            return Ok(PushOutcome::Replaced);
        }

        // Buffer cap check — count existing rows for this group.
        let count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM mls_epoch_ahead WHERE group_id = ?1",
                params![&group_id[..]],
                |r| r.get(0),
            )
            .map_err(|e| {
                MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e))
            })?;

        if (count as usize) >= MAX_EPOCH_AHEAD_BUFFER {
            // Drop newest (i.e. drop the incoming).
            log::warn!(
                "EpochCatchupBuffer: group_id={} buffer full ({} rows), dropping newest \
                 (this group may be stuck — consider RequestRejoin)",
                hex::encode(&group_id[..4]),
                count
            );
            tx.commit()
                .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
            return Ok(PushOutcome::Discarded);
        }

        tx.execute(
            "INSERT INTO mls_epoch_ahead \
             (group_id, epoch, dispatch_id, msg_blob, received_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&group_id[..], msg_epoch as i64, &dispatch_id, &msg_bytes, now_ms as i64],
        )
        .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
        tx.commit()
            .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
        Ok(PushOutcome::Inserted)
    }

    /// Drain any newly-processable buffered messages after a
    /// commit-merge that advanced the group's epoch.
    ///
    /// Pseudocode:
    ///
    /// ```text
    /// loop:
    ///     for (epoch, ent) in scan(group_id) where epoch <= group.epoch():
    ///         feed_to_openmls(ent.msg_blob)
    ///         delete(ent)
    ///         if produced an Application: append to result
    ///         if produced a staged Commit: merge_staged_commit() — restart loop
    ///     break when no progress
    /// ```
    ///
    /// Returns the list of decrypted Application messages produced
    /// during the drain (in the order they were processed). Staged
    /// commits found in the buffer are auto-merged; the caller does
    /// not need to look at them. Errors during processing are
    /// logged and the offending row is *deleted* (a permanently
    /// undecryptable message is data loss either way, and re-trying
    /// across reconnects would just amplify CPU cost).
    pub fn drain_when_ready(
        &self, group: &mut MlsGroupHandle, provider: &PromtuzMlsProvider,
    ) -> Result<Vec<ProcessedApplicationMessage>, MlsGroupError> {
        let group_id = group.group_id();
        let mut output = Vec::new();
        let mut iterations = 0usize;

        loop {
            if iterations >= EPOCH_CATCHUP_LIMIT {
                log::warn!(
                    "EpochCatchupBuffer::drain_when_ready: group_id={} hit \
                     EPOCH_CATCHUP_LIMIT = {}, stopping drain (potentially stuck)",
                    hex::encode(&group_id[..4]),
                    EPOCH_CATCHUP_LIMIT
                );
                break;
            }
            iterations += 1;

            let current_epoch = group.epoch();

            // Find a candidate row to process: epoch <= current.
            // We pick the *oldest* (lowest received_at_ms) to give
            // commits priority — they're most likely to unblock epoch advance.
            let candidate: Option<(Vec<u8>, Vec<u8>, u64)> = {
                let conn = self.conn.lock();
                conn.query_row(
                    "SELECT dispatch_id, msg_blob, epoch FROM mls_epoch_ahead \
                     WHERE group_id = ?1 AND epoch <= ?2 \
                     ORDER BY epoch ASC, received_at_ms ASC LIMIT 1",
                    params![&group_id[..], current_epoch as i64],
                    |r| {
                        let did: Vec<u8> = r.get(0)?;
                        let blob: Vec<u8> = r.get(1)?;
                        let ep: i64 = r.get(2)?;
                        Ok((did, blob, ep as u64))
                    },
                )
                .ok()
            };

            let Some((dispatch_id, msg_blob, msg_epoch)) = candidate else {
                break; // no progressable rows
            };

            // Process FIRST, delete on success (or on a hard
            // parse-error that means re-trying would never make
            // progress — those still get deleted to keep the buffer
            // from filling with poison messages). The previous
            // "delete-then-process" pattern lost a message on any
            // transient panic during processing.
            //
            // Helper closure: delete the row by `(group_id, dispatch_id)`.
            let delete_row = |dispatch_id: &[u8]| -> Result<(), MlsGroupError> {
                let conn = self.conn.lock();
                conn.execute(
                    "DELETE FROM mls_epoch_ahead \
                     WHERE group_id = ?1 AND dispatch_id = ?2",
                    params![&group_id[..], dispatch_id],
                )
                .map_err(|e| {
                    MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e))
                })?;
                Ok(())
            };

            // Feed to openmls.
            let in_msg = match mls_message_from_bytes(&msg_blob) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!(
                        "EpochCatchupBuffer: malformed buffered msg dropped: {e}"
                    );
                    // Hard parse error → no point retrying; delete.
                    delete_row(&dispatch_id)?;
                    continue;
                }
            };
            let proto = match in_msg.try_into_protocol_message() {
                Ok(p) => p,
                Err(e) => {
                    log::warn!(
                        "EpochCatchupBuffer: buffered msg is not a ProtocolMessage: {e:?}"
                    );
                    delete_row(&dispatch_id)?;
                    continue;
                }
            };

            match group.process_incoming(provider, proto) {
                Ok(ProcessedMessageContent::ApplicationMessage(app)) => {
                    output.push(application_to_processed(app, dispatch_id.clone(), msg_epoch));
                    delete_row(&dispatch_id)?;
                }
                Ok(ProcessedMessageContent::StagedCommitMessage(staged)) => {
                    match group.merge_staged_commit(provider, *staged) {
                        Ok(()) => {
                            delete_row(&dispatch_id)?;
                        }
                        Err(e) => {
                            log::warn!(
                                "EpochCatchupBuffer: merge_staged_commit failed: {e}"
                            );
                            // Don't delete — let the next drain re-attempt
                            // (in case state catches up). The
                            // EPOCH_CATCHUP_LIMIT loop bound prevents
                            // an infinite spin on a permanently-stuck
                            // commit.
                        }
                    }
                }
                Ok(ProcessedMessageContent::ProposalMessage(_))
                | Ok(ProcessedMessageContent::ExternalJoinProposalMessage(_)) => {
                    // Proposals from the buffer have no caller; they
                    // should already have been rolled into a commit
                    // by the time they appear here. Drop silently.
                    delete_row(&dispatch_id)?;
                }
                Err(e) => {
                    log::warn!(
                        "EpochCatchupBuffer: process_incoming error on buffered msg: {e}"
                    );
                    // Permanent crypto failure → delete (re-trying
                    // won't help) — but log loudly so the user sees
                    // it. Soft failures (transient I/O during
                    // openmls processing) would loop here, bounded
                    // by EPOCH_CATCHUP_LIMIT.
                    delete_row(&dispatch_id)?;
                }
            }
        }

        Ok(output)
    }

    /// Drop every buffered row for a group (forget-contact cascade).
    pub fn purge_group(&self, group_id: &[u8; 32]) -> Result<(), MlsGroupError> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM mls_epoch_ahead WHERE group_id = ?1",
            params![&group_id[..]],
        )
        .map_err(|e| MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e)))?;
        Ok(())
    }

    /// Number of rows currently buffered for a group. Test helper +
    /// future UI metric.
    pub fn buffered_count(&self, group_id: &[u8; 32]) -> Result<usize, MlsGroupError> {
        let conn = self.conn.lock();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mls_epoch_ahead WHERE group_id = ?1",
                params![&group_id[..]],
                |r| r.get(0),
            )
            .map_err(|e| {
                MlsGroupError::Storage(super::types::PromtuzMlsStorageError::Sqlite(e))
            })?;
        Ok(n as usize)
    }
}

/// Bridge an openmls `ApplicationMessage` into our public-facing
/// struct.
fn application_to_processed(
    app: ApplicationMessage, dispatch_id: Vec<u8>, epoch: u64,
) -> ProcessedApplicationMessage {
    ProcessedApplicationMessage {
        dispatch_id,
        plaintext: app.into_bytes(),
        epoch,
    }
}

/// Wall-clock millis since Unix epoch. We only use this for ordering
/// within the buffer and for diagnostic logs; not exposed to the
/// network.
fn unix_now_ms() -> u64 {
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::mls::apply_mls_migrations;
    use crate::mls::group::mls_message_to_bytes;
    use crate::mls::group::PROMTUZ_CIPHERSUITE;
    use openmls::prelude::*;
    use rusqlite::Connection;

    /// Build a single shared connection so the provider and the
    /// buffer point at the same DB. (Fresh cache per test; SQLite
    /// in-memory.)
    fn build_provider_and_buffer() -> (PromtuzMlsProvider, EpochCatchupBuffer) {
        let mut conn = Connection::open_in_memory().expect("in-memory db");
        apply_mls_migrations(&mut conn);
        let conn = Arc::new(Mutex::new(conn));
        let provider = PromtuzMlsProvider::new(conn.clone());
        let buffer = EpochCatchupBuffer::new(conn);
        (provider, buffer)
    }

    /// Test fixture identical to the one in `group::tests`.
    struct Party {
        ipk: [u8; 32],
        sig_kp: openmls_basic_credential::SignatureKeyPair,
    }
    impl Party {
        fn new(provider: &PromtuzMlsProvider, seed: u8) -> Self {
            let ipk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
                .verifying_key()
                .to_bytes();
            let sig_kp = openmls_basic_credential::SignatureKeyPair::new(SignatureScheme::ED25519)
                .expect("sig kp");
            sig_kp.store(provider.storage()).expect("store sig kp");
            Self { ipk, sig_kp }
        }
    }

    fn make_kp(provider: &PromtuzMlsProvider, party: &Party) -> KeyPackage {
        let credential = BasicCredential::new(party.ipk.to_vec());
        let cwk = CredentialWithKey {
            credential: credential.into(),
            signature_key: party.sig_kp.public().into(),
        };
        let bundle = KeyPackage::builder()
            .leaf_node_capabilities(Capabilities::new(
                None,
                Some(&[PROMTUZ_CIPHERSUITE]),
                None,
                None,
                None,
            ))
            .build(PROMTUZ_CIPHERSUITE, provider, &party.sig_kp, cwk)
            .expect("build kp");
        bundle.key_package().clone()
    }

    /// Build alice → adds bob → returns alice_group, bob_group at
    /// the same epoch. Both groups in *one* shared provider so
    /// alice can send and bob can receive without cross-DB plumbing.
    fn pair_setup() -> (PromtuzMlsProvider, EpochCatchupBuffer, MlsGroupHandle, MlsGroupHandle, Party, Party)
    {
        let (provider_a, buffer_a) = build_provider_and_buffer();
        // bob has his own provider (his KP bundle lives in his
        // storage so openmls can find his init/enc keys).
        let mut conn_b = Connection::open_in_memory().expect("b conn");
        apply_mls_migrations(&mut conn_b);
        let conn_b = Arc::new(Mutex::new(conn_b));
        let provider_b = PromtuzMlsProvider::new(conn_b.clone());

        let alice = Party::new(&provider_a, 1);
        let bob = Party::new(&provider_b, 2);

        let mut alice_group = MlsGroupHandle::create(
            &provider_a,
            &alice.sig_kp,
            &alice.ipk,
            alice.sig_kp.public(),
            &[0xAA; 32],
        )
        .expect("create alice group");
        let bob_kp = make_kp(&provider_b, &bob);
        let (_commit, welcome) = alice_group
            .add_members(&provider_a, &alice.sig_kp, &[bob_kp])
            .expect("add bob");
        alice_group.merge_pending_commit(&provider_a).expect("merge");

        // Round-trip through tls_codec to extract the inner Welcome
        // (openmls 0.8 gates `into_welcome()` behind test-utils
        // feature; cfg(test) only fires for openmls itself, not
        // dependents).
        use openmls::prelude::tls_codec::Deserialize as _;
        use openmls::prelude::tls_codec::Serialize as _;
        let bytes = welcome.tls_serialize_detached().expect("ser");
        let in_msg = MlsMessageIn::tls_deserialize_exact(&bytes).expect("deser");
        let welcome_msg = match in_msg.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            other => panic!("expected welcome, got {other:?}"),
        };
        let join = MlsGroupJoinConfig::default();
        let staged = StagedWelcome::new_from_welcome(&provider_b, &join, welcome_msg, None)
            .expect("staged");
        let bob_group = MlsGroupHandle::wrap(staged.into_group(&provider_b).expect("into"));
        // Move bob's group to alice's provider so we can use a
        // single buffer. We have to load it from alice's storage —
        // easiest: store alice's provider and bob's separately, but
        // build the buffer on top of alice's provider since bob is
        // the one buffering (alice sends, bob receives).
        //
        // Replan: we want bob to be the buffering side. Use bob's
        // provider for the buffer.
        drop(buffer_a);
        let buffer_b = EpochCatchupBuffer::new(conn_b);
        (provider_a, buffer_b, alice_group, bob_group, alice, bob)
    }

    // -------------------------------------------------------------
    // Test 2: Process the commit; drain_when_ready returns the
    // buffered application message.
    // -------------------------------------------------------------
    #[test]
    fn drain_when_ready_returns_now_processable_application() {
        let (provider_a, buffer, mut alice_group, mut bob_group, alice, _bob) = pair_setup();
        // bob's provider is implicit in `buffer`; re-construct
        // a handle to it for openmls calls.
        let bob_conn = buffer.conn.clone();
        let provider_b = PromtuzMlsProvider::new(bob_conn);

        // Alice sends an Application message at the current epoch.
        // Bob is NOT going to process it directly; instead, we
        // simulate "received before the load-bearing thing" by
        // pushing it into the buffer, then immediately draining
        // (epoch already matches → drain processes it).
        let plaintext = b"buffered-then-drained";
        let alice_msg = alice_group
            .create_application_message(&provider_a, &alice.sig_kp, plaintext)
            .expect("encrypt");
        let bytes = mls_message_to_bytes(&alice_msg).expect("ser");
        let dispatch_id = vec![0xDE, 0xAD, 0xBE, 0xEF];
        // Push at the *current* epoch — the drain will pick it up.
        let outcome = buffer
            .push(&bob_group, bytes, bob_group.epoch(), dispatch_id.clone())
            .expect("push");
        assert_eq!(outcome, PushOutcome::Inserted);

        let drained = buffer
            .drain_when_ready(&mut bob_group, &provider_b)
            .expect("drain");
        assert_eq!(drained.len(), 1, "exactly one application drained");
        assert_eq!(drained[0].plaintext, plaintext);
        assert_eq!(drained[0].dispatch_id, dispatch_id);
        assert_eq!(buffer.buffered_count(&bob_group.group_id()).unwrap(), 0);
    }

    // -------------------------------------------------------------
    // Test 3: Buffer overflow drops newest (push returns Discarded),
    // logs warning.
    // -------------------------------------------------------------
    #[test]
    fn buffer_overflow_drops_newest() {
        let (provider_b, buffer, _alice_group, bob_group, _alice, _bob) = pair_setup();
        let _ = provider_b;
        // Fill to the cap with synthetic msg blobs (each with a
        // distinct dispatch_id so insert doesn't dedupe).
        let gid = bob_group.group_id();
        for i in 0..MAX_EPOCH_AHEAD_BUFFER {
            // Use a non-zero, distinct 4-byte id.
            let id = (i as u32).to_be_bytes().to_vec();
            let outcome = buffer
                .push(&bob_group, vec![0x42; 16], 999, id)
                .expect("push");
            assert_eq!(outcome, PushOutcome::Inserted);
        }
        assert_eq!(
            buffer.buffered_count(&gid).unwrap(),
            MAX_EPOCH_AHEAD_BUFFER
        );

        // Push one more — should be Discarded.
        let outcome = buffer
            .push(&bob_group, vec![0x99; 16], 999, vec![0xFF; 4])
            .expect("push (cap)");
        assert_eq!(outcome, PushOutcome::Discarded);
        assert_eq!(
            buffer.buffered_count(&gid).unwrap(),
            MAX_EPOCH_AHEAD_BUFFER,
            "count unchanged after Discarded"
        );
    }

    // -------------------------------------------------------------
    // Test 4: Persistence — buffered message survives buffer
    // re-construction over the same connection.
    // -------------------------------------------------------------
    #[test]
    fn buffered_message_persists_across_buffer_reconstructions() {
        let (provider_b, buffer, _alice_group, bob_group, _alice, _bob) = pair_setup();
        let _ = provider_b;
        let gid = bob_group.group_id();
        let id = vec![0xBE, 0xEF];

        let outcome = buffer
            .push(&bob_group, vec![0x55; 32], 7, id.clone())
            .expect("push");
        assert_eq!(outcome, PushOutcome::Inserted);

        // Reconstruct a fresh EpochCatchupBuffer over the same
        // connection (simulates a libcore restart that preserves
        // the SQLite file).
        let buffer2 = EpochCatchupBuffer::new(buffer.conn.clone());
        assert_eq!(buffer2.buffered_count(&gid).unwrap(), 1);

        // Idempotent re-push of the same dispatch_id is Replaced.
        let outcome2 = buffer2
            .push(&bob_group, vec![0x55; 32], 7, id)
            .expect("push2");
        assert_eq!(outcome2, PushOutcome::Replaced);
        assert_eq!(buffer2.buffered_count(&gid).unwrap(), 1);
    }

    // -------------------------------------------------------------
    // Test 5: Stale message (epoch < current, malformed bytes) gets
    // dropped during drain rather than persisting forever.
    // -------------------------------------------------------------
    #[test]
    fn stale_or_malformed_buffered_messages_are_dropped_on_drain() {
        let (provider_a, buffer, _alice_group, mut bob_group, _alice, _bob) = pair_setup();
        let _ = provider_a;
        let bob_conn = buffer.conn.clone();
        let provider_b = PromtuzMlsProvider::new(bob_conn);

        // Push a malformed (non-MLS) blob at the current epoch.
        let dispatch_id = vec![0xCA, 0xFE];
        let outcome = buffer
            .push(
                &bob_group,
                b"definitely-not-an-mls-frame".to_vec(),
                bob_group.epoch(),
                dispatch_id,
            )
            .expect("push");
        assert_eq!(outcome, PushOutcome::Inserted);
        assert_eq!(buffer.buffered_count(&bob_group.group_id()).unwrap(), 1);

        // Drain — the malformed row is silently dropped.
        let drained = buffer
            .drain_when_ready(&mut bob_group, &provider_b)
            .expect("drain");
        assert_eq!(drained.len(), 0, "malformed blob produces no application");
        assert_eq!(
            buffer.buffered_count(&bob_group.group_id()).unwrap(),
            0,
            "row was deleted regardless of decode failure"
        );
    }
}
