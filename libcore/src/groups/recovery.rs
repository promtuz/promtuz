//! Group transport and recovery, shared by live delivery and reconnect.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use log::warn;
use common::crypto::verify_versioned;
use common::proto::client_rel::Wake;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::GroupMessage;
use common::proto::mls_wire::MlsApplicationEnvelopeP;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::group_envelope_signing_input;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::utils::now_secs;
use ed25519_dalek::SigningKey;
use openmls::prelude::MlsMessageIn;
use openmls::prelude::ProcessedMessageContent;
use openmls::prelude::tls_codec::Deserialize as _;
use rusqlite::params;

use crate::data::conversation::Conversation;
use crate::db::outbox::OpType;
use crate::messaging::receive::InboundDecoded;
use crate::messaging::send::SealedMessage;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::mls::recovery::Candidate;
use crate::mls::recovery::DispatchJob;
use crate::mls::recovery::Received;
use crate::mls::recovery::Replay;
use crate::mls::recovery::Transaction;
use crate::mls::recovery::{
    self as journal,
};
use crate::state::core;

pub type Copy = ([u8; 32], [u8; 16], OpType, Vec<u8>);

/// Keep the deletion intent in the messages transaction. If the process dies
/// between databases, reconnect finishes erasing the duplicate replay content.
pub fn clear_marker(conversation: &[u8; 16]) -> Result<Option<(String, u64)>> {
    let Some(gid) = Conversation::group_of(conversation) else { return Ok(None) };
    Ok(Some((
        format!("group_replay_clear:{}", hex::encode(gid)),
        journal::replay_watermark(&PromtuzMlsProvider::shared(), &gid)?,
    )))
}

pub fn finish_clears() -> Result<()> {
    for (key, value) in crate::data::app_prefs::with_prefix("group_replay_clear:") {
        let gid: [u8; 32] = hex::decode(key.trim_start_matches("group_replay_clear:"))?
            .try_into()
            .map_err(|_| anyhow!("invalid cleared group"))?;
        journal::clear_replay(&PromtuzMlsProvider::shared(), &gid, value.parse()?)?;
        core().db.messages()
            .lock()
            .execute("DELETE FROM app_prefs WHERE key=?1 AND value=?2", params![key, value])?;
    }
    Ok(())
}

pub fn copies(jobs: &[DispatchJob]) -> Vec<Copy> {
    jobs.iter()
        .map(|j| {
            (
                j.recipient,
                j.id,
                OpType::from_u8(j.kind as u8).expect("known dispatch kind"),
                j.frame.clone(),
            )
        })
        .collect()
}

pub fn welcome_envelope(
    welcome: common::proto::mls_wire::WelcomeEnvelopeP, history: &[u8], signer: &SigningKey,
) -> Result<MlsEnvelopeP> {
    use ed25519_dalek::Signer;
    let signature =
        signer.sign(&common::proto::mls_wire::group_welcome_signing_input(&welcome, history));
    Ok(MlsEnvelopeP::GroupWelcome {
        welcome,
        history: history.to_vec().into(),
        signature: signature.to_bytes().into(),
    })
}

pub fn addressed(
    sealed: &SealedMessage, logical_id: [u8; 16], recipients: &[[u8; 32]], signer: &SigningKey,
    kind: OpType, wake: Wake, ttl_ms: u64,
) -> Result<Vec<DispatchJob>> {
    let id =
        sealed.branch.as_ref().map(|b| journal::dispatch_id(b, &logical_id)).unwrap_or(logical_id);
    let me = signer.verifying_key().to_bytes();
    recipients
        .iter()
        .map(|to| {
            Ok(DispatchJob {
                recipient: *to,
                id,
                logical_id,
                kind: kind as i64,
                frame: crate::delivery::prepare_dispatch(
                    to,
                    &me,
                    signer,
                    &id,
                    sealed.address_to(to, signer)?,
                    wake,
                    ttl_ms,
                )?,
            })
        })
        .collect()
}

/// The group refuses the send, so a retry gets the same answer.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Refused(&'static str);

/// Seal a logical payload once, retaining its original recipient set for
/// recovery. Replays never disclose pre-join content to newly added members.
pub fn queue_locked(
    outbox: &parking_lot::Mutex<rusqlite::Connection>, provider: &PromtuzMlsProvider,
    gid: [u8; 32], id: [u8; 16], payload: Vec<u8>, recipients: &[[u8; 32]], signer: &SigningKey,
    kind: OpType, wake: Wake, ttl_ms: u64, durable: bool,
) -> Result<Vec<Copy>> {
    let mut operation =
        Transaction::open(provider, gid, None)?.ok_or_else(|| anyhow!("no group state"))?;
    if kind == OpType::Message {
        let conversation =
            Conversation::for_group(&gid).ok_or_else(|| anyhow!("group has no conversation"))?;
        ensure!(
            crate::data::message::Message::get_by_dispatch(&conversation, &id)
                .is_some_and(|m| !m.inner.deleted),
            "message was deleted"
        );
    }
    let me = signer.verifying_key().to_bytes();
    ensure!(
        operation.group.application_is_permitted(&me, &payload),
        Refused("this action is not permitted in the group")
    );
    let roster = operation.group.roster();
    ensure!(roster.contains(&me), Refused("you are no longer in this group"));
    let recipients: Vec<_> =
        recipients.iter().copied().filter(|r| roster.contains(r) && *r != me).collect();
    ensure!(!recipients.is_empty(), Refused("no original recipients remain in the group"));
    let leaf = crate::messaging::session::leaf_signer_for_group(&operation.provider, &operation.group, &me)?;
    let bytes = postcard::to_allocvec(&GroupMessage {
        id:      id.into(),
        payload: payload.clone().into(),
    })?;
    let mut sealed = crate::messaging::send::seal_application_message(
        &operation.provider,
        &mut operation.group,
        &leaf,
        &bytes,
    )?;
    sealed.branch = Some(operation.parent);
    let jobs = addressed(&sealed, id, &recipients, signer, kind, wake, ttl_ms)?;
    // Requests are regenerated from pending intent, and forwarded Welcomes are
    // specific to a commit. Neither is a replayable application action.
    let replayable = durable
        && !matches!(
            AppPayload::deser(&payload),
            Ok(AppPayload::GroupRequest(_)
                | AppPayload::GroupWelcome { .. }
                | AppPayload::GroupInvitation { .. })
        );
    let replay = Replay { id, payload, recipients, wake: wake as i64, kind: kind as i64 };
    operation.publish(
        None,
        if durable { &jobs } else { &[] },
        replayable.then_some(&replay),
        None,
    )?;
    let mut copies = copies(&jobs);
    crate::delivery::enqueue_batch_in(&mut outbox.lock(), &mut copies)?;
    if durable {
        materialized(provider, &jobs)?;
    }
    Ok(copies)
}

pub fn materialized(provider: &PromtuzMlsProvider, jobs: &[DispatchJob]) -> Result<()> {
    let conn = provider.storage().connection();
    let mut conn = conn.lock();
    let tx = conn.transaction()?;
    for job in jobs {
        tx.execute(
            "DELETE FROM mls_dispatch_jobs WHERE recipient=?1 AND dispatch_id=?2",
            params![job.recipient, job.id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Complete any publication interrupted between the MLS and outbox databases.
pub fn flush_jobs(provider: &PromtuzMlsProvider, gid: [u8; 32]) -> Result<Vec<Copy>> {
    let jobs = {
        let conn = provider.storage().connection();
        let conn = conn.lock();
        let mut q = conn.prepare("SELECT recipient,dispatch_id,kind,frame FROM mls_dispatch_jobs WHERE group_id=?1 ORDER BY rowid")?;
        q.query_map([gid], |r| {
            Ok(DispatchJob {
                recipient:  r.get(0)?,
                id:         r.get(1)?,
                logical_id: r.get(1)?,
                kind:       r.get(2)?,
                frame:      r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut copies = copies(&jobs);
    crate::delivery::enqueue_batch(&mut copies)?;
    materialized(provider, &jobs)?;
    Ok(copies)
}

pub async fn dispatch(copies: Vec<Copy>) -> usize {
    let mut delivered = 0;
    for (to, id, kind, bytes) in copies {
        if crate::delivery::dispatch_queued(core().session().as_deref(), &to, &id, kind, &bytes)
            .await
            == crate::delivery::LastOutcome::Durable
        {
            delivered += 1;
        }
    }
    delivered
}

pub fn receive(
    provider: &PromtuzMlsProvider, ours: &[u8; 32], sender: [u8; 32], branch: [u8; 32],
    envelope: MlsApplicationEnvelopeP, sealed_proof: Option<Vec<u8>>, dispatch: [u8; 16],
    accepted_at_ms: u64,
) -> Result<InboundDecoded> {
    let gid = envelope.group_id.0;
    let _operation = journal::operation_lock(&gid).lock();
    ensure!(
        envelope.version == common::proto::mls_wire::MLS_ENVELOPE_VERSION,
        "unsupported group envelope"
    );
    ensure!(
        envelope.mls_message.0.len() <= common::proto::mls_wire::MAX_FRAMED_MLS_BYTES,
        "group message too large"
    );
    verify_versioned(&sender, &envelope.sender_sig.0, |v| {
        group_envelope_signing_input(v, ours, &gid, envelope.epoch, &branch, &envelope.mls_message.0)
    })?;
    let Some(mut operation) = Transaction::open(provider, gid, Some(branch))? else {
        let Some(group) = MlsGroupHandle::load(provider, &gid)? else {
            if let Some(conversation) = Conversation::for_group(&gid)
                && Conversation::active_members(&conversation).contains(&sender)
            {
                super::member_requests::refresh(conversation)?;
            }
            return Ok(InboundDecoded::ApplicationNoGroup { group_id: gid });
        };
        if envelope.epoch.saturating_sub(group.epoch()) > common::proto::mls_wire::MAX_EPOCH_AHEAD {
            return Ok(InboundDecoded::ApplicationStale);
        }
        let bytes = MlsEnvelopeP::GroupApplication {
            branch:  branch.into(),
            message: envelope,
            proof:   sealed_proof.map(Into::into),
        }
        .ser()?;
        let conn = provider.storage().connection();
        let conn = conn.lock();
        let count: u32 = conn.query_row(
            "SELECT COUNT(*) FROM mls_branch_inbox WHERE group_id=?1",
            [gid],
            |r| r.get(0),
        )?;
        ensure!(count < crate::mls::MAX_EPOCH_AHEAD_BUFFER as u32, "group recovery inbox full");
        conn.execute("INSERT OR IGNORE INTO mls_branch_inbox(group_id,branch,sender,dispatch_id,accepted_at_ms,envelope) VALUES(?1,?2,?3,?4,?5,?6)",
            params![gid, branch, sender, dispatch, accepted_at_ms, bytes])?;
        drop(conn);
        // A data frame commonly arrives before its commit. Give ordinary
        // reordering time to drain before asking for fresh membership keys.
        if count == 0 {
            core().spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                if let Some(conversation) = Conversation::for_group(&gid) {
                    super::resume(conversation);
                }
            });
        }
        return Ok(InboundDecoded::ApplicationBuffered);
    };
    ensure!(operation.group.epoch() == envelope.epoch, "group epoch does not match its branch");
    let parent_group = MlsGroupHandle::load(&operation.provider, &gid)?
        .ok_or_else(|| anyhow!("parent state missing"))?;
    let proof = sealed_proof
        .map(
            |bytes| -> Result<(
                Option<common::proto::mls_wire::GroupBranch>,
                common::proto::mls_wire::GroupBranch,
            )> {
                let bytes = crate::mls::branch_proof::open(
                    &operation.provider,
                    &parent_group,
                    crate::mls::branch_proof::COMMIT_LABEL,
                    &bytes,
                )?;
                Ok(postcard::from_bytes(&bytes)?)
            },
        )
        .transpose()?;
    let message = MlsMessageIn::tls_deserialize_exact(&envelope.mls_message.0)?
        .try_into_protocol_message()
        .map_err(|e| anyhow!("not an MLS protocol message: {e:?}"))?;
    let processed = match operation.group.process_incoming(&operation.provider, message) {
        Ok(p) => p,
        Err(e) if e.is_spent_secret() => {
            // The MLS transaction may have committed just before a messages
            // database failure. Redelivery must finish those durable effects.
            drop(operation);
            if let Some(conversation) = Conversation::for_group(&gid) {
                reconcile_events(provider, gid, conversation)?;
                deliver_plaintext(provider, gid, conversation)?;
            }
            return Ok(InboundDecoded::ApplicationUndecryptable);
        },
        Err(e) => return Err(e.into()),
    };
    let author = processed.sender;
    let mut changed = None;
    let mut candidate = None;
    let mut received = None;
    match processed.content {
        ProcessedMessageContent::ApplicationMessage(message) => {
            ensure!(proof.is_none(), "application carries a commit proof");
            ensure!(author == sender, "application carrier is not its author");
            let message: GroupMessage = postcard::from_bytes(&message.into_bytes())?;
            if operation.group.application_is_permitted(&author, &message.payload.0) {
                received = Some(Received {
                    author,
                    id: message.id.0,
                    accepted_at_ms,
                    payload: message.payload.0,
                });
            }
        },
        ProcessedMessageContent::StagedCommitMessage(staged) => {
            let before = operation
                .group
                .group_meta()
                .ok_or_else(|| anyhow!("not a group chat"))?
                .effective();
            if let Ok(Some(signed)) = operation.group.commit_is_permitted(&staged, author)
                && let common::proto::mls_wire::GroupChange::MemberRequest(request) = signed.change
            {
                let history = journal::history(provider, &gid, operation.parent)?;
                ensure!(
                    !crate::mls::branch_proof::member_requests(&history)?
                        .iter()
                        .any(|old| old.who == request.who && old.nonce == request.nonce),
                    "replayed member request"
                );
            }
            match operation.group.merge_staged_commit_if_permitted(
                &operation.provider,
                *staged,
                author,
            )? {
                crate::mls::CommitOutcome::Refused => return Ok(InboundDecoded::ApplicationStale),
                crate::mls::CommitOutcome::Merged(change) => changed = change,
            }
            let (bootstrap, proof) =
                proof.ok_or_else(|| anyhow!("group commit has no recovery proof"))?;
            crate::mls::branch_proof::verify_commit(
                &parent_group,
                &operation.group,
                &proof,
                &author,
                &envelope.mls_message.0,
            )?;
            if !journal::registered(provider, &gid)? {
                journal::accept_root(
                    provider,
                    &parent_group,
                    &bootstrap.ok_or_else(|| anyhow!("group upgrade has no founder proof"))?,
                )?;
            }
            candidate = Some(Candidate {
                rank:    before.role(&author),
                message: envelope.mls_message.0,
                change:  changed.as_ref().map(|c| c.signed.clone()),
                proof:   Some(proof),
            });
        },
        _ => return Ok(InboundDecoded::ApplicationStale),
    }
    let result = operation.publish(candidate, &[], None, received.as_ref())?;
    if let Some(group) = MlsGroupHandle::load(provider, &gid)? {
        let conversation = crate::messaging::welcome::home_for_group(&group, &author)?;
        if result.needs_recovery {
            super::member_requests::refresh(conversation)?;
        }
        Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
        if result.canonical
            && let Some(changed) = changed
        {
            super::changed(
                conversation,
                &changed,
                crate::messaging::receive::accepted_at_secs(accepted_at_ms),
            )?;
        }
        reconcile_events(provider, gid, conversation)?;
        deliver_plaintext(provider, gid, conversation)?;
        if result.previous != result.head {
            super::resume(conversation);
        }
    }
    Ok(InboundDecoded::ApplicationBuffered)
}

/// Applies every staged payload in arrival order. Only storage fails it: a rejected payload is
/// dropped.
pub fn deliver_plaintext(
    provider: &PromtuzMlsProvider, gid: [u8; 32], conversation: [u8; 16],
) -> Result<()> {
    deliver_with(provider, gid, |m| {
        crate::messaging::receive::receive_application_content(
            conversation,
            m.author,
            m.id,
            m.accepted_at_ms,
            &m.payload,
        )
    })
}

/// [`deliver_plaintext`] with `apply` storing each payload.
fn deliver_with(
    provider: &PromtuzMlsProvider, gid: [u8; 32], mut apply: impl FnMut(&Received) -> Result<()>,
) -> Result<()> {
    let pending = provider.storage().with_conn(|conn| -> Result<Vec<Received>> {
        let mut q = conn.prepare("SELECT sender,dispatch_id,accepted_at_ms,payload FROM mls_group_received WHERE group_id=?1 AND applied=0 ORDER BY rowid")?;
        Ok(q.query_map([gid], |r| {
            Ok(Received {
                author:         r.get(0)?,
                id:             r.get(1)?,
                accepted_at_ms: r.get(2)?,
                payload:        r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
    })?;
    for message in pending {
        if let Err(e) = apply(&message) {
            if crate::utils::is_storage_error(&e) {
                return Err(e);
            }
            warn!(
                "MESSAGE: dropped a rejected payload from {} in {}: {e:#}",
                hex::encode(&message.author[..4]),
                hex::encode(&gid[..4])
            );
        }
        provider.storage().with_conn(|conn| {
            conn.execute("UPDATE mls_group_received SET applied=1,payload=X'' WHERE group_id=?1 AND sender=?2 AND dispatch_id=?3",
                params![gid, message.author, message.id])
        })?;
    }
    Ok(())
}

pub async fn reconcile(conversation: [u8; 16]) -> Result<()> {
    finish_clears()?;
    let provider = PromtuzMlsProvider::shared();
    let gid = super::require_group(&conversation)?;
    let (me, signer) = super::local_signer()?;
    dispatch(flush_jobs(&provider, gid)?).await;
    drain(&provider, &me, gid)?;
    recover_missing_branches(&provider, gid, conversation)?;
    {
        let _operation = journal::operation_lock(&gid).lock();
        reconcile_events(&provider, gid, conversation)?;
        restore_requests(&provider, gid, conversation, &me)?;
        deliver_plaintext(&provider, gid, conversation)?;
    }
    if super::member_requests::left(&gid) {
        journal::prune(&provider, &gid, now_secs())?;
        return Ok(());
    }
    for replay in journal::replay_needed(&provider, &gid)? {
        let kind = OpType::from_u8(replay.kind as u8)
            .ok_or_else(|| anyhow!("unknown group dispatch kind"))?;
        let Some(group) = MlsGroupHandle::load(&provider, &gid)? else { break };
        let available = kind != OpType::Message
            || crate::data::message::Message::get_by_dispatch(&conversation, &replay.id)
                .is_some_and(|m| !m.inner.deleted);
        if !available
            || !group.roster().contains(&me)
            || !group.application_is_permitted(&me, &replay.payload)
            || !replay.recipients.iter().any(|r| group.roster().contains(r))
        {
            journal::discard_replay(&provider, &gid, &replay.id)?;
            if let Some(message) =
                crate::data::message::Message::get_by_dispatch(&conversation, &replay.id)
            {
                crate::data::message::Message::mark_failed(&message.inner.id);
            }
            continue;
        }
        let wake = match replay.wake {
            0 => Wake::No,
            1 => Wake::Message,
            _ => Wake::Call,
        };
        match crate::messaging::send::queue_application(
            core().db.outbox(),
            &provider,
            gid,
            replay.id,
            replay.payload,
            &replay.recipients,
            &signer,
            kind,
            wake,
            0,
            true,
        ) {
            Ok(copies) => {
                dispatch(copies).await;
            },
            Err(e) => log::warn!("GROUP: replay remains pending: {e}"),
        }
    }
    journal::prune(&provider, &gid, now_secs())?;
    Ok(())
}

fn recover_missing_branches(
    provider: &PromtuzMlsProvider, gid: [u8; 32], conversation: [u8; 16],
) -> Result<()> {
    let senders = {
        let connection = provider.storage().connection();
        let conn = connection.lock();
        let mut q = conn.prepare(
            "SELECT DISTINCT i.sender FROM mls_branch_inbox i LEFT JOIN mls_branches b ON b.group_id=i.group_id AND b.branch=i.branch WHERE i.group_id=?1 AND i.received_at<=?2 AND (b.branch IS NULL OR length(b.snapshot)=0)",
        )?;
        q.query_map(params![gid, now_secs().saturating_sub(60)], |r| {
            r.get::<_, [u8; 32]>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let members = Conversation::active_members(&conversation);
    if senders.iter().any(|s| members.contains(s)) {
        super::member_requests::refresh(conversation)?;
    }
    Ok(())
}

fn reconcile_events(
    provider: &PromtuzMlsProvider, gid: [u8; 32], conversation: [u8; 16],
) -> Result<()> {
    use sha2::Digest;
    use sha2::Sha256;
    let Some(group) = MlsGroupHandle::load(provider, &gid)? else { return Ok(()) };
    let history = journal::history(provider, &gid, group.branch_id())?;
    let mut accepted = Vec::new();
    for pair in history.windows(2) {
        let Some(change) = crate::mls::branch_proof::change_between(&pair[0], &pair[1])? else {
            continue;
        };
        accepted.push(Sha256::digest(postcard::to_allocvec(&change.signed)?).into());
        // Joining history authenticates the group; it is not chat history.
        let timestamp = {
            use rusqlite::OptionalExtension;
            provider.storage().with_conn(|conn| conn.query_row(
                "SELECT created_at FROM mls_branches WHERE group_id=?1 AND branch=?2 AND change_blob IS NOT NULL",
                params![gid, pair[1].branch.0], |r| r.get::<_, u64>(0)).optional())?
        };
        if let Some(timestamp) = timestamp {
            super::changed(conversation, &change, timestamp)?;
        }
    }
    crate::data::message::Message::reconcile_group_events(&conversation, &accepted)
}

fn drain(provider: &PromtuzMlsProvider, me: &[u8; 32], gid: [u8; 32]) -> Result<()> {
    loop {
        let pending = {
            let conn = provider.storage().connection();
            let conn = conn.lock();
            let mut q = conn.prepare("SELECT i.sender,i.dispatch_id,i.accepted_at_ms,i.envelope FROM mls_branch_inbox i JOIN mls_branches b ON b.group_id=i.group_id AND b.branch=i.branch WHERE i.group_id=?1 AND length(b.snapshot)>0 ORDER BY i.rowid")?;
            q.query_map([gid], |r| {
                Ok((
                    r.get::<_, [u8; 32]>(0)?,
                    r.get::<_, [u8; 16]>(1)?,
                    r.get::<_, u64>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if pending.is_empty() {
            return Ok(());
        }
        let mut progressed = false;
        for (sender, id, accepted_at_ms, bytes) in pending {
            if let MlsEnvelopeP::GroupApplication { branch, message, proof } =
                MlsEnvelopeP::deser(&bytes)?
            {
                if let Err(e) = receive(
                    provider,
                    me,
                    sender,
                    branch.0,
                    message,
                    proof.map(|p| p.0),
                    id,
                    accepted_at_ms,
                ) {
                    log::warn!("GROUP: buffered frame could not be applied: {e}");
                    continue;
                }
            }
            provider.storage().connection().lock().execute(
                "DELETE FROM mls_branch_inbox WHERE group_id=?1 AND sender=?2 AND dispatch_id=?3",
                params![gid, sender, id],
            )?;
            progressed = true;
        }
        if !progressed { return Ok(()); }
    }
}

fn restore_requests(
    provider: &PromtuzMlsProvider, gid: [u8; 32], conversation: [u8; 16], me: &[u8; 32],
) -> Result<()> {
    let lost = {
        let conn = provider.storage().connection();
        let conn = conn.lock();
        let path = journal::canonical_path(&conn, &gid)?;
        let mut q = conn.prepare("SELECT b.branch,b.change_blob FROM mls_branches b WHERE b.group_id=?1 AND b.change_blob IS NOT NULL AND NOT EXISTS(SELECT 1 FROM mls_recovery_retries r WHERE r.group_id=b.group_id AND r.branch=b.branch) ORDER BY b.epoch")?;
        q.query_map([gid], |r| Ok((r.get::<_, [u8; 32]>(0)?, r.get::<_, Vec<u8>>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(branch, _)| !path.contains(branch))
            .collect::<Vec<_>>()
    };
    for (branch, bytes) in lost {
        let signed: common::proto::mls_wire::SignedChange = postcard::from_bytes(&bytes)?;
        if signed.by.0 == *me
            && !matches!(
                signed.change,
                common::proto::mls_wire::GroupChange::Takeover
                    | common::proto::mls_wire::GroupChange::Handover { .. }
                    | common::proto::mls_wire::GroupChange::Upgrade
            )
        {
            if let common::proto::mls_wire::GroupChange::Add { who } = &signed.change {
                for who in who {
                    crate::data::app_prefs::set(&super::asked_key(&conversation, &who.0), "1")?;
                }
            }
            super::remember(&conversation, signed.change)?;
        }
        provider.storage().connection().lock().execute(
            "INSERT OR IGNORE INTO mls_recovery_retries(group_id,branch) VALUES(?1,?2)",
            params![gid, branch],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedCore;
    use crate::test_support::net::Device;

    /// A receive holds the group while it merges and narrates a commit, so reconciling waits for
    /// it instead of acting on the history that commit is changing.
    #[tokio::test]
    async fn reconcile_waits_for_a_receive_holding_the_group() {
        let scope = ScopedCore::new();
        let me = crate::test_support::data::identity(&scope.core.db.identity().lock(), 0x5A);
        let gid = [0x5B; 32];
        let conversation = Conversation::join_group(&me.verifying_key().to_bytes(), &[]).unwrap();
        Conversation::bind_group(&conversation, &gid).unwrap();
        // A change on a branch that lost the election, which reconciling marks as retried.
        let change = common::proto::mls_wire::SignedChange {
            by:     [0x5C; 32].into(),
            epoch:  0,
            branch: [0; 32].into(),
            change: common::proto::mls_wire::GroupChange::Takeover,
            sig:    common::types::bytes::Bytes([0; 64]),
        };
        let mls = scope.core.db.mls();
        let root = "INSERT INTO mls_recovery_roots VALUES (?1, ?2)";
        mls.lock().execute(root, params![gid, [1u8; 32]]).unwrap();
        mls.lock()
            .execute(
                "INSERT INTO mls_branches (group_id, branch, parent, epoch, rank, commit_hash, \
                 snapshot, change_blob) VALUES (?1, ?2, ?3, 1, 0, X'', X'', ?4)",
                params![gid, [2u8; 32], [3u8; 32], postcard::to_allocvec(&change).unwrap()],
            )
            .unwrap();
        let retried = |mls: &parking_lot::Mutex<rusqlite::Connection>| -> u32 {
            let sql = "SELECT COUNT(*) FROM mls_recovery_retries";
            mls.lock().query_row(sql, [], |r| r.get(0)).unwrap()
        };

        let (held, holding) = std::sync::mpsc::channel();
        let operation = journal::operation_lock(&gid);
        let receive = std::thread::spawn({
            let mls = mls.clone();
            move || {
                let _operation = operation.lock();
                held.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                retried(&mls)
            }
        });
        holding.recv().unwrap();
        reconcile(conversation).await.unwrap();
        assert_eq!(receive.join().unwrap(), 0, "nothing was reconciled while the receive ran");
        assert_eq!(retried(&mls), 1);
    }

    /// A rejected payload is dropped so it cannot block the group's queue. A failed store keeps
    /// the rest staged, and the next delivery applies them first, in arrival order.
    #[test]
    fn a_rejected_payload_is_dropped_and_a_failed_store_keeps_the_rest_staged() {
        let device = Device::new(7);
        let gid = [7; 32];
        let message = |n: u8| Received {
            author:         [n; 32],
            id:             [n; 16],
            accepted_at_ms: u64::from(n),
            payload:        vec![n],
        };
        let stage = |batch: &[Received]| {
            for m in batch {
                m.stage(&device.db.mls().lock(), &gid, &[0; 32]).unwrap();
            }
        };
        let mut stored = Vec::new();
        stage(&[message(1), message(2), message(3)]);
        let failed = deliver_with(&device.provider, gid, |m| match m.payload[0] {
            1 => Err(anyhow!("revision not permitted")),
            3 => Err(rusqlite::Error::InvalidQuery.into()),
            n => {
                stored.push(n);
                Ok(())
            },
        });
        assert!(crate::utils::is_storage_error(&failed.unwrap_err()));
        assert_eq!(stored, [2]);

        stage(&[message(4)]);
        deliver_with(&device.provider, gid, |m| {
            stored.push(m.payload[0]);
            Ok(())
        })
        .unwrap();
        assert_eq!(stored, [2, 3, 4]);
        deliver_with(&device.provider, gid, |_| panic!("nothing is left to apply")).unwrap();
    }
}
