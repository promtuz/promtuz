//! Group chats: founding one, and every later change to who is in it, who runs it and by what
//! rules. The committer carries other members' signed requests; every member checks each commit.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::GroupRequest;
use common::proto::mls_wire::GroupRules;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::SignedChange;
use common::proto::mls_wire::SystemEvent;
use common::proto::mls_wire::group_change_signing_input;
use common::types::bytes::Bytes;
use common::utils::now_secs;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use log::info;
use log::warn;
use serde::Deserialize;
use serde::Serialize;

use crate::data::conversation::Conversation;
use crate::data::conversation::KIND_GROUP;
use crate::data::conversation::ROLE_ADMIN;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::db::messages::SYSTEM_ADDED;
use crate::db::messages::SYSTEM_LEFT;
use crate::db::messages::SYSTEM_REMOVED;
use crate::db::messages::SYSTEM_ROLE;
use crate::db::messages::SYSTEM_RULES;
use crate::db::outbox::OpType;
use crate::events::Emittable;
use crate::events::messaging::MessageEv;
use crate::messaging::send::SealedMessage;
use crate::messaging::send_control;
use crate::messaging::session::MlsContext;
use crate::mls::Changed;
use crate::mls::EpochCatchupBuffer;
use crate::mls::GroupMeta;
use crate::mls::GroupState;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::mls::policy;
use crate::quic::dht_client::DhtClientError;
use crate::state::core;

pub(crate) mod member_requests;
pub(crate) mod migration;
pub(crate) mod recovery;

#[derive(Default)]
pub(crate) struct Groups {
    /// One commit at a time on this device. Two built from the same epoch would fork the group.
    membership:               tokio::sync::Mutex<()>,
    /// One follow-up of pending requests at a time, so a burst of commits asks the committer once
    /// rather than once per commit.
    follow_up:                tokio::sync::Mutex<()>,
    migration_retries:        parking_lot::Mutex<std::collections::HashSet<[u8; 16]>>,
    pub(crate) picture_write: tokio::sync::Mutex<()>,
}

/// How long an admin's request waits on a silent committer before the admin
/// takes its role.
const TAKEOVER_AFTER_SECS: u64 = 24 * 60 * 60;

/// A request nobody carried in this long is dropped. A leave never is.
const REQUEST_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Binds `$ctx` to a live [`MlsContext`], or bails without a relay: a membership change has no
/// offline path.
macro_rules! with_mls {
    ($ctx:ident, $body:block) => {{
        let Some(client) = core().session().map(|s| s.dht.clone()) else {
            bail!("not connected to a relay; reconnect before changing the group");
        };
        let provider = PromtuzMlsProvider::shared();
        let stash = KeyPackageStash::new(core().db.mls());
        let buffer = EpochCatchupBuffer::new(core().db.mls());
        let $ctx = MlsContext {
            provider: &provider,
            stash:    &stash,
            buffer:   &buffer,
            dht:      client.as_ref(),
        };
        $body
    }};
}

/// We become owner and committer. Every invitation is durably queued before this returns.
pub async fn create_group(title: String, members: Vec<[u8; 32]>) -> Result<[u8; 16]> {
    if members.is_empty() {
        bail!("a group needs at least one other member");
    }
    if members.len() + 1 > crate::mls::MAX_GROUP_MEMBERS {
        bail!("a group is limited to {} members", crate::mls::MAX_GROUP_MEMBERS);
    }
    let our_ipk = local_ipk()?;
    if members.contains(&our_ipk) {
        bail!("you are already in the group you are creating");
    }
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    with_mls!(ctx, {
        // Every KeyPackage first: finding a member without one after minting would leave a
        // half-built group behind.
        let mut kps = Vec::with_capacity(members.len());
        for m in &members {
            let (kp, kp_ref) = crate::messaging::session::fetch_verified_keypackage(&ctx, m, true)
                .await
                .map_err(|e| no_keys_error(m, e))?;
            kps.push((*m, kp, kp_ref));
        }

        let group_id = crate::messaging::session::mint_group_id(&our_ipk);
        let (leaf_kp, cwk) = crate::messaging::session::build_self_credential(&ipk_signer)
            .map_err(|e| anyhow!("build credential: {e}"))?;
        leaf_kp.store(ctx.provider.storage()).map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

        // The meta tells joiners this is a group, not a pair, and who runs it. It rides in the MLS
        // group context, so no relay can strip it from the Welcome.
        let meta = GroupMeta::founded(title.clone(), our_ipk);
        MlsGroupHandle::create(ctx.provider, &leaf_kp, cwk, &group_id, Some(&meta))
            .map_err(|e| anyhow!("create group: {e}"))?;
        let root = load_group(ctx.provider, &group_id)?;
        crate::mls::recovery::ensure_root(ctx.provider, &root, &ipk_signer)?;
        let root_history =
            crate::mls::recovery::history(ctx.provider, &group_id, root.branch_id())?;
        member_requests::anchor(&group_id, root_history[0].branch.0)?;
        // The held transaction ends before the network is awaited.
        {
            let mut operation = crate::mls::recovery::Transaction::open(ctx.provider, group_id, None)?
                .ok_or_else(|| anyhow!("group state missing"))?;
            let (commit, welcome) = operation.group.add_members(
                &operation.provider,
                &leaf_kp,
                &kps.iter().map(|(_, kp, _)| kp.clone()).collect::<Vec<_>>(),
            )?;
            let sealed = SealedMessage::from_mls_out(&commit, group_id, operation.group.epoch())?;
            operation.group.merge_pending_commit(&operation.provider)?;
            let mut candidate = crate::mls::recovery::Candidate {
                rank:    policy::ROLE_OWNER,
                message: sealed.mls_bytes,
                change:  None,
                proof:   None,
            };
            let proof = crate::mls::branch_proof::sign(
                &operation.group,
                Some(operation.parent),
                candidate.rank,
                &candidate.message,
                &ipk_signer,
            )?;
            candidate.proof = Some(proof.clone());
            let history =
                crate::mls::recovery::next_history(ctx.provider, &group_id, operation.parent, &proof)?;
            let encrypted_history = crate::mls::branch_proof::seal(
                &operation.provider,
                &operation.group,
                crate::mls::branch_proof::INVITATION_LABEL,
                &postcard::to_allocvec(&history)?,
            )?;
            let jobs = kps
                .iter()
                .map(|(member, _, kp_ref)| {
                    welcome_job(
                        &welcome,
                        group_id,
                        our_ipk,
                        *member,
                        *kp_ref,
                        &ipk_signer,
                        &encrypted_history,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            operation.publish(Some(candidate), &jobs, None, None)?;
        }
        let group = load_group(ctx.provider, &group_id)?;

        let conversation = Conversation::create_group(&title, &members)?;
        Conversation::bind_group(&conversation, &group_id)?;
        Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
        recovery::dispatch(recovery::flush_jobs(ctx.provider, group_id)?).await;
        info!("GROUP: created \"{title}\" with {} members", members.len() + 1);
        // Members who never paired with us may not know our name.
        crate::messaging::welcome::introduce_ourselves(conversation);
        Ok(conversation)
    })
}

/// No pre-join history: forward secrecy means its keys no longer exist. Returns whether it is
/// done or was asked of the committer.
pub async fn add_members(conversation: [u8; 16], who: Vec<[u8; 32]>) -> Result<bool> {
    require_group(&conversation)?;
    if !Conversation::has_signed_rules(&conversation) {
        for w in who {
            legacy_add(conversation, w).await?;
        }
        return Ok(true);
    }
    // Marks the adds we asked for, so we deliver only the Welcomes we wanted
    // delivered.
    for w in &who {
        crate::data::app_prefs::set(&asked_key(&conversation, w), "1")?;
    }
    ask(conversation, GroupChange::Add { who: who.into_iter().map(Into::into).collect() }).await
}

/// The removing commit refreshes the group's keys, so their device cannot read what follows.
pub async fn remove_member(conversation: [u8; 16], who: [u8; 32]) -> Result<bool> {
    require_group(&conversation)?;
    if !Conversation::has_signed_rules(&conversation) {
        require_owner(&conversation, &local_ipk()?)?;
        evict(conversation, who, Some(SystemEvent::Removed { who: who.into() })).await?;
        return Ok(true);
    }
    ask(conversation, GroupChange::Remove { who: who.into() }).await
}

/// `role` is 0 member, 1 admin or 2 owner.
pub async fn set_role(conversation: [u8; 16], who: [u8; 32], role: u8) -> Result<bool> {
    require_signed_rules(&conversation)?;
    ask(conversation, GroupChange::Role { who: who.into(), role }).await
}

pub async fn set_rules(conversation: [u8; 16], rules: GroupRules) -> Result<bool> {
    require_signed_rules(&conversation)?;
    ask(conversation, GroupChange::Rules(rules)).await
}

/// The conversation and its history stay. The signed departure survives epoch changes and local
/// deletion, and the chat shows us gone at once while delivery finishes.
pub async fn leave(conversation: [u8; 16]) -> Result<()> {
    require_group(&conversation)?;
    if !Conversation::has_signed_rules(&conversation) {
        return legacy_leave(conversation).await;
    }
    let me = local_ipk()?;
    member_requests::leave(conversation)?;
    if Conversation::active_members(&conversation) == [me] {
        // Nobody left to tell.
        Conversation::deactivate_member(&conversation, &me)?;
        forget_group(&conversation);
        return Ok(());
    }
    remember(&conversation, GroupChange::Leave { successor: None })?;
    Ok(())
}

/// Carries `change` when we commit for the group; otherwise asks the committer, again at every
/// new epoch until it is carried. Returns whether it is done.
async fn ask(conversation: [u8; 16], change: GroupChange) -> Result<bool> {
    let me = local_ipk()?;
    let state = require_state(&conversation)?;
    permitted(&state, &Conversation::active_members(&conversation), &me, &change)
        .map_err(|e| anyhow!("{e}"))?;
    remember(&conversation, change.clone())?;
    if state.committer == me {
        commit_change(conversation, sign(&conversation, change)?).await?;
        return Ok(true);
    }
    send_change(conversation, change, state.committer).await?;
    Ok(false)
}

/// Whether the committer would carry `change` for `me`. Removing the committer is allowed: it
/// hands its role over first.
fn permitted(
    state: &GroupState, members: &[[u8; 32]], me: &[u8; 32], change: &GroupChange,
) -> std::result::Result<(), &'static str> {
    let mut state = state.clone();
    if matches!(change, GroupChange::Remove { who } if who.0 == state.committer) {
        state.committer = *me;
    }
    let probe = SignedChange {
        by:     (*me).into(),
        epoch:  0,
        branch: [0; 32].into(),
        change: change.clone(),
        sig:    Bytes([0; 64]),
    };
    policy::apply(&state, members, &state.committer.clone(), &probe).map(|_| ())
}

pub(crate) fn requested(conversation: [u8; 16], from: [u8; 32], request: GroupRequest) {
    core().spawn(async move {
        let done = match request {
            GroupRequest::Change(signed) => carry_request(conversation, from, signed).await,
            GroupRequest::Sync => {
                catch_up(conversation, from).await;
                Ok(())
            },
            GroupRequest::Ready => Ok(()),
            GroupRequest::RecoveryReady => ready_from(conversation, from).await,
            GroupRequest::Add { .. } | GroupRequest::Remove { .. } => Ok(()),
        };
        if let Err(e) = done {
            warn!("GROUP: could not carry a request from {}: {e}", hex::encode(&from[..4]));
        }
    });
}

async fn carry_request(conversation: [u8; 16], from: [u8; 32], signed: SignedChange) -> Result<()> {
    if signed.by.0 != from {
        bail!("the change was signed by someone else");
    }
    let me = local_ipk()?;
    let state = require_state(&conversation)?;
    if state.committer != me {
        // They ask again once they see who commits now.
        return Ok(());
    }
    // A committer can't commit its own removal. Hand the role to whoever may
    // remove us, and they carry it.
    if matches!(&signed.change, GroupChange::Remove { who } if who.0 == me) {
        let provider = crate::mls::PromtuzMlsProvider::shared();
        load_group(&provider, &require_group(&conversation)?)?
            .verify_change(&signed)
            .map_err(|e| anyhow!("{e}"))?;
        let as_them = GroupState { committer: from, ..state.clone() };
        policy::apply(&as_them, &Conversation::active_members(&conversation), &from, &signed)
            .map_err(|e| anyhow!("{e}"))?;
        return commit_change(
            conversation,
            sign(&conversation, GroupChange::Handover { to: from.into() })?,
        )
        .await;
    }
    commit_change(conversation, signed).await
}

/// Commit `signed` as the group's committer, or as the admin taking its place.
async fn commit_change(conversation: [u8; 16], signed: SignedChange) -> Result<()> {
    let _one = core().groups.membership.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;
    with_mls!(ctx, {
        if let GroupChange::MemberRequest(request) = &signed.change
            && member_requests::completed(ctx.provider, &group_id, request)?
        {
            return Ok(());
        }
        // Validate before fetching, then validate again against the snapshot
        // after the network await. An incoming commit may have advanced us.
        load_settled(ctx.provider, &conversation, &group_id)?
            .state_after_change(&our_ipk, &signed)
            .map_err(|e| anyhow!("{e}"))?;
        let mut joiners = Vec::new();
        let adding = match &signed.change {
            GroupChange::Add { who } => who.clone(),
            GroupChange::MemberRequest(request)
                if request.action == common::proto::mls_wire::GroupMemberAction::Refresh =>
            {
                vec![request.who]
            },
            _ => Vec::new(),
        };
        for w in adding {
            let (kp, kp_ref) = crate::messaging::session::fetch_verified_keypackage(&ctx, &w.0, true)
                .await
                .map_err(|e| no_keys_error(&w.0, e))?;
            joiners.push((w.0, kp, kp_ref));
        }
        let copies = {
            let _operation = crate::mls::recovery::operation_lock(&group_id).lock();
            let root = load_group(ctx.provider, &group_id)?;
            crate::mls::recovery::ensure_root(ctx.provider, &root, &ipk_signer)?;
            let root_history =
                crate::mls::recovery::history(ctx.provider, &group_id, root.branch_id())?;
            member_requests::anchor(&group_id, root_history[0].branch.0)?;
            let mut operation =
                crate::mls::recovery::Transaction::open(ctx.provider, group_id, None)?
                    .ok_or_else(|| anyhow!("group state missing"))?;
            let group = &mut operation.group;
            let provider = &operation.provider;
            let meta = group.group_meta().ok_or_else(|| anyhow!("not a group chat"))?;
            let before = meta.effective();
            let state = group.state_after_change(&our_ipk, &signed).map_err(|e| anyhow!("{e}"))?;
            if group.member_count() + joiners.len()
                - usize::from(matches!(signed.change, GroupChange::MemberRequest(_)))
                > crate::mls::MAX_GROUP_MEMBERS
            {
                bail!("a group is limited to {} members", crate::mls::MAX_GROUP_MEMBERS);
            }
            let removed = match &signed.change {
                GroupChange::Remove { who } => Some(who.0),
                GroupChange::Leave { .. } => Some(signed.by.0),
                GroupChange::MemberRequest(request) => Some(request.who.0),
                _ => None,
            };
            let removes = removed
                .map(|who| {
                    group
                        .member_index_by_ipk(&who)
                        .ok_or_else(|| anyhow!("that member is not in this group"))
                })
                .transpose()?
                .into_iter()
                .collect();
            let recipients: Vec<_> = group.roster().into_iter().filter(|r| *r != our_ipk).collect();
            let title = Conversation::get(&conversation).map(|c| c.title).unwrap_or(meta.title);
            let next = GroupMeta { title, founder: meta.founder, state: Some(state) };
            let leaf = leaf_for(provider, group, &our_ipk)?;
            let (commit, welcome) = group.commit_meta(
                provider,
                &leaf,
                &next,
                joiners.iter().map(|(_, kp, _)| kp.clone()).collect(),
                removes,
            )?;
            let mut sealed = SealedMessage::from_mls_out(&commit, group_id, group.epoch())?;
            sealed.branch = Some(operation.parent);
            let mut candidate = crate::mls::recovery::Candidate {
                rank:    before.role(&our_ipk),
                message: sealed.mls_bytes.clone(),
                change:  Some(signed.clone()),
                proof:   None,
            };
            group.merge_pending_commit(provider)?;
            let proof = crate::mls::branch_proof::sign(
                group,
                Some(operation.parent),
                candidate.rank,
                &candidate.message,
                &ipk_signer,
            )?;
            let bootstrap =
                matches!(signed.change, GroupChange::Upgrade).then(|| root_history[0].clone());
            sealed.proof = Some(crate::mls::branch_proof::seal(
                ctx.provider,
                &root,
                crate::mls::branch_proof::COMMIT_LABEL,
                &postcard::to_allocvec(&(bootstrap, &proof))?,
            )?);
            candidate.proof = Some(proof.clone());
            let history = crate::mls::recovery::next_history(
                ctx.provider,
                &group_id,
                operation.parent,
                &proof,
            )?;
            crate::mls::branch_proof::verify_history(&group_id, &history, group)?;
            let encrypted_history = crate::mls::branch_proof::seal(
                provider,
                group,
                crate::mls::branch_proof::INVITATION_LABEL,
                &postcard::to_allocvec(&history)?,
            )?;
            let mut jobs = recovery::addressed(
                &sealed,
                crate::data::message::next_dispatch_id(),
                &recipients,
                &ipk_signer,
                OpType::Control,
                common::proto::client_rel::Wake::Message,
                0,
            )?;
            if let Some(welcome) = welcome {
                if signed.by.0 == our_ipk {
                    for (who, _, kp_ref) in &joiners {
                        jobs.push(welcome_job(
                            &welcome,
                            group_id,
                            our_ipk,
                            *who,
                            *kp_ref,
                            &ipk_signer,
                            &encrypted_history,
                        )?);
                    }
                } else {
                    use common::proto::pack::Packer;
                    use openmls::prelude::tls_codec::Serialize as _;
                    let blob = crate::mls::encode_welcome(&welcome)?;
                    for (who, _, kp_ref) in &joiners {
                        let id = crate::data::message::next_dispatch_id();
                        let payload = AppPayload::GroupInvitation {
                            who:     (*who).into(),
                            kp_ref:  (*kp_ref).into(),
                            welcome: blob.clone(),
                            history: encrypted_history.clone(),
                        }
                        .ser()?;
                        let wrapped = common::proto::mls_wire::GroupMessage {
                            id:      id.into(),
                            payload: payload.into(),
                        };
                        let message = group.create_application_message(
                            provider,
                            &leaf,
                            &postcard::to_allocvec(&wrapped)?,
                        )?;
                        let sealed = SealedMessage {
                            group_id,
                            epoch: group.epoch(),
                            branch: Some(group.branch_id()),
                            mls_bytes: message.tls_serialize_detached()?,
                            proof: None,
                        };
                        jobs.extend(recovery::addressed(
                            &sealed,
                            id,
                            &[signed.by.0],
                            &ipk_signer,
                            OpType::Control,
                            common::proto::client_rel::Wake::Message,
                            0,
                        )?);
                    }
                }
            }
            // Commit, every Welcome, and every forwarded Welcome become durable
            // together. A failed recipient cannot roll back a commit seen by others.
            operation.publish(Some(candidate), &jobs, None, None)?;
            let group = load_group(ctx.provider, &group_id)?;
            Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
            changed(
                conversation,
                &Changed { signed: signed.clone(), before },
                now_secs(),
            )?;
            recovery::flush_jobs(ctx.provider, group_id)?
        };
        recovery::dispatch(copies).await;
        resume(conversation);
        info!("GROUP: committed {:?} in {}", signed.change, hex::encode(&conversation[..4]));
        Ok(())
    })
}

fn welcome_job(
    welcome: &openmls::prelude::MlsMessageOut, group_id: [u8; 32], from: [u8; 32], who: [u8; 32],
    kp_ref: [u8; 32], signer: &SigningKey, history: &[u8],
) -> Result<crate::mls::recovery::DispatchJob> {
    use common::proto::pack::Packer;
    let env =
        crate::mls::make_welcome_envelope(welcome.clone(), group_id, from, who, kp_ref, signer)?;
    let payload = recovery::welcome_envelope(env, history, signer)?.ser()?;
    let id = crate::data::message::next_dispatch_id();
    Ok(crate::mls::recovery::DispatchJob {
        recipient: who,
        id,
        logical_id: id,
        kind: OpType::Welcome as i64,
        frame: crate::delivery::prepare_dispatch(
            &who,
            &from,
            signer,
            &id,
            payload,
            common::proto::client_rel::Wake::Message,
            0,
        )?,
    })
}

/// Narrate a change a commit made, the same way on every member's device, and
/// introduce ourselves to anyone it added.
pub(crate) fn changed(conversation: [u8; 16], changed: &Changed, ts: u64) -> Result<()> {
    use sha2::Digest;
    use sha2::Sha256;
    let Some(me) = Identity::local_ipk() else { return Ok(()) };
    let by = match &changed.signed.change {
        GroupChange::MemberRequest(request) => request.who.0,
        _ => changed.signed.by.0,
    };
    let mut rows = Vec::new();
    match &changed.signed.change {
        GroupChange::Add { who } => {
            for w in who {
                rows.push((SYSTEM_ADDED, hex::encode(w.0)));
            }
        },
        GroupChange::Remove { who } => rows.push((SYSTEM_REMOVED, hex::encode(who.0))),
        GroupChange::Leave { .. } => rows.push((SYSTEM_LEFT, hex::encode(by))),
        GroupChange::Role { who, role } => {
            rows.push((SYSTEM_ROLE, format!("{}:{role}", hex::encode(who.0))))
        },
        GroupChange::Rules(rules) => {
            let was = changed.before.rules;
            for (name, now, then) in [
                ("add", rules.members_add, was.members_add),
                ("edit", rules.members_edit, was.members_edit),
                ("send", rules.members_send, was.members_send),
                ("appoint", rules.admins_appoint, was.admins_appoint),
            ] {
                if now != then {
                    rows.push((SYSTEM_RULES, format!("{name}:{}", u8::from(now))));
                }
            }
        },
        GroupChange::MemberRequest(request) => {
            if request.action == common::proto::mls_wire::GroupMemberAction::Leave {
                rows.push((SYSTEM_LEFT, hex::encode(request.who.0)));
            }
        },
        GroupChange::Handover { .. } | GroupChange::Takeover | GroupChange::Upgrade => {},
    }
    let id: [u8; 32] = Sha256::digest(postcard::to_allocvec(&changed.signed)?).into();
    if let Some(saved) =
        crate::data::message::Message::record_group_change(conversation, id, by, &rows, ts)?
    {
        for row in saved {
            crate::events::messaging::MessageEv::Received {
                id: row.inner.id,
                conversation,
                sender: by,
                content: row.inner.content,
                timestamp: ts,
            }
            .emit();
        }
        if let GroupChange::Add { who } = &changed.signed.change {
            for who in who.iter().filter(|w| w.0 != me) {
                crate::messaging::welcome::introduce_ourselves_to(conversation, who.0);
            }
        }
    }
    Ok(())
}

/// Stores a membership or title line locally, then sends it to every member. Best-effort: the
/// Commit is what moves the roster, the line only narrates it.
pub(crate) async fn announce(conversation: [u8; 16], event: SystemEvent) {
    let Some(our_ipk) = Identity::local_ipk() else { return };
    let (code, actor, target) = system_row(&event, our_ipk);
    narrate(conversation, code, actor, &target, now_secs());
    if let Err(e) = send_control(conversation, AppPayload::System(event)).await {
        warn!("GROUP: system event not delivered: {e}");
    }
}

pub(crate) fn narrate(conversation: [u8; 16], code: u8, actor: [u8; 32], target: &str, ts: u64) {
    let did = crate::data::message::next_dispatch_id();
    let ours = Identity::get().is_some_and(|i| i.ipk() == actor);
    match Message::save_system(conversation, actor, &did, code, target, ts, ours) {
        Ok(Some(row)) => MessageEv::Received {
            id: row.inner.id,
            conversation,
            sender: actor,
            content: target.to_string(),
            timestamp: ts,
        }
        .emit(),
        Ok(None) => {},
        Err(e) => warn!("GROUP: could not record a system row: {e}"),
    }
}

/// How a system event is stored: its code, who did it, and its target, which
/// is a member's hex for the membership events and the new name for a rename.
pub(crate) fn system_row(event: &SystemEvent, author: [u8; 32]) -> (u8, [u8; 32], String) {
    use crate::db::messages::SYSTEM_ADDED;
    use crate::db::messages::SYSTEM_LEFT;
    use crate::db::messages::SYSTEM_REMOVED;
    use crate::db::messages::SYSTEM_TITLED;
    match event {
        SystemEvent::Added { who } => (SYSTEM_ADDED, author, hex::encode(who.0)),
        SystemEvent::Left { who } => (SYSTEM_LEFT, author, hex::encode(who.0)),
        SystemEvent::Removed { who } => (SYSTEM_REMOVED, author, hex::encode(who.0)),
        SystemEvent::Titled { title } => (SYSTEM_TITLED, author, title.clone()),
        SystemEvent::AddedBy { who, by } => (SYSTEM_ADDED, by.0, hex::encode(who.0)),
        SystemEvent::RemovedBy { who, by } => (SYSTEM_REMOVED, by.0, hex::encode(who.0)),
    }
}

/// A change we asked the committer for, kept until it's carried.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    change: GroupChange,
    since:  u64,
}

fn pending_key(conversation: &[u8; 16]) -> String {
    format!("group_pending:{}", hex::encode(conversation))
}

fn pending(conversation: &[u8; 16]) -> Vec<Pending> {
    crate::data::app_prefs::get(&pending_key(conversation))
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| postcard::from_bytes(&b).ok())
        .unwrap_or_default()
}

fn keep_pending(conversation: &[u8; 16], list: &[Pending]) -> Result<()> {
    if list.is_empty() {
        return crate::data::app_prefs::remove(&pending_key(conversation));
    }
    crate::data::app_prefs::set(
        &pending_key(conversation),
        &hex::encode(postcard::to_allocvec(list)?),
    )
}

fn remember(conversation: &[u8; 16], change: GroupChange) -> Result<()> {
    let mut list = pending(conversation);
    if !list.iter().any(|p| p.change == change) {
        list.push(Pending { change, since: now_secs() });
    }
    keep_pending(conversation, &list)
}

pub(crate) fn forget_requests(conversation: &[u8; 16]) {
    let _ = keep_pending(conversation, &[]);
}

/// We asked to leave and the committer hasn't carried it yet. The chat already
/// shows us gone.
pub(crate) fn is_leaving(conversation: &[u8; 16]) -> bool {
    pending(conversation).iter().any(|p| matches!(p.change, GroupChange::Leave { .. }))
}

pub(crate) fn delete_pending(conversation: &[u8; 16]) -> bool {
    crate::data::app_prefs::get(&format!("group_delete:{}", hex::encode(conversation))).is_some()
}

/// Hide the chat and remove its history, retaining the state needed to finish
/// a leave after another commit makes our original request stale.
pub(crate) fn delete_after_leave(conversation: &[u8; 16]) -> Result<()> {
    let gid = Conversation::group_of(conversation);
    let _operation = gid.as_ref().map(|g| crate::mls::recovery::operation_lock(g).lock());
    let clear = recovery::clear_marker(conversation)?;
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    if let Some((key, through)) = clear {
        tx.execute(
            "INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,?2)",
            (key, through.to_string()),
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO app_prefs(key,value) VALUES(?1,'1')",
        [format!("group_delete:{}", hex::encode(conversation))],
    )?;
    let orphaned = Conversation::clear_history_tx(&tx, conversation)?;
    tx.commit()?;
    crate::data::media::unlink_orphaned(core(), &conn, &orphaned);
    drop(conn);
    recovery::finish_clears()
}

pub(crate) fn may_post(conversation: &[u8; 16], who: &[u8; 32]) -> bool {
    !is_leaving(conversation) && Conversation::state(conversation).is_none_or(|s| s.may_send(who))
}

/// The group moved on: follow up on our requests.
pub(crate) fn resume(conversation: [u8; 16]) {
    core().spawn(async move {
        if let Err(e) = follow_up(conversation).await {
            warn!("GROUP: could not follow up in {}: {e}", hex::encode(&conversation[..4]));
        }
    });
}

/// Runs once the queue has drained. Follows up in every group and, as an admin, takes over one
/// whose committer has left a request of ours waiting a day.
pub(crate) fn on_reconnect() {
    member_requests::resume();
    core().spawn(async move {
        let Ok(me) = local_ipk() else { return };
        if let Err(e) = recovery::finish_clears() {
            warn!("GROUP: replay cleanup remains pending: {e}");
        }
        let provider = PromtuzMlsProvider::shared();
        // Publication may have completed just before the messages database
        // acquired its conversation mapping. Home it before draining jobs.
        if let Ok(gids) = crate::mls::recovery::recoverable_groups(&provider) {
            for gid in gids {
                if !member_requests::left(&gid)
                    && Conversation::for_group(&gid).is_none()
                    && let Ok(Some(group)) = MlsGroupHandle::load(&provider, &gid)
                    && let Err(e) = crate::messaging::welcome::home_for_group(&group, &me)
                {
                    warn!("GROUP: could not restore chat: {e}");
                }
            }
        }
        let now = now_secs();
        let groups = Conversation::list()
            .into_iter()
            .filter(|c| c.kind == KIND_GROUP && c.mls_group_id.is_some())
            .map(|c| c.id);
        for conversation in groups {
            let state = Conversation::state(&conversation);
            let members = Conversation::active_members(&conversation);
            let stale = state.as_ref().is_some_and(|state| {
                pending(&conversation).iter().any(|p| {
                    now.saturating_sub(p.since) >= TAKEOVER_AFTER_SECS
                        && wanted(state, &members, &me, p, now)
                })
            });
            if stale
                && Conversation::has_signed_rules(&conversation)
                && let Some(state) = Conversation::state(&conversation)
                && state.committer != me
                && state.role(&me) >= ROLE_ADMIN
            {
                info!(
                    "GROUP: taking over {} from a committer that went quiet",
                    hex::encode(&conversation[..4])
                );
                let taken = match sign(&conversation, GroupChange::Takeover) {
                    Ok(signed) => commit_change(conversation, signed).await,
                    Err(e) => Err(e),
                };
                if let Err(e) = taken {
                    warn!("GROUP: could not take over: {e}");
                }
            }
            if let Err(e) = follow_up(conversation).await {
                warn!("GROUP: could not follow up in {}: {e}", hex::encode(&conversation[..4]));
            }
        }
    });
}

/// Drops requests that are done or no longer allowed and asks again for the rest, signed for the
/// current epoch; carried directly once this device commits.
async fn follow_up(conversation: [u8; 16]) -> Result<()> {
    let _one = core().groups.follow_up.lock().await;
    if migration::follow_up(conversation).await? { return Ok(()); }
    // Backups preserve the public group anchor and roster, not epoch secrets.
    // Request fresh keys without waiting for someone to send into the lost epoch.
    if let Some(gid) = Conversation::group_of(&conversation)
        && member_requests::known_anchor(&gid).is_some()
        && MlsGroupHandle::load(&PromtuzMlsProvider::shared(), &gid)?.is_none() {
        if !member_requests::left(&gid) { member_requests::refresh(conversation)?; }
        return Ok(());
    }
    if Conversation::has_signed_rules(&conversation) {
        recovery::reconcile(conversation).await?;
    }
    if !Conversation::has_signed_rules(&conversation) {
        announce_ready(conversation).await?;
        return maybe_upgrade(conversation).await;
    }
    let mut list = pending(&conversation);
    if list.is_empty() {
        return Ok(());
    }
    let me = local_ipk()?;
    let state = require_state(&conversation)?;
    let members = Conversation::active_members(&conversation);
    let leaving = list.iter().any(|p| matches!(p.change, GroupChange::Leave { .. }));
    if leaving && !members.contains(&me) {
        // Our leave was carried.
        keep_pending(&conversation, &[])?;
        if let Some(gid) = Conversation::group_of(&conversation) {
            member_requests::leave_carried(&gid)?;
        }
        forget_group(&conversation);
        if delete_pending(&conversation) {
            Conversation::delete(&conversation)?;
            crate::data::app_prefs::remove(&format!("group_delete:{}", hex::encode(conversation)))?;
        }
        return Ok(());
    }
    let now = now_secs();
    list.retain(|p| {
        if let GroupChange::MemberRequest(request) = &p.change
            && let Some(gid) = Conversation::group_of(&conversation)
            && member_requests::completed(&PromtuzMlsProvider::shared(), &gid, request)
                .unwrap_or(false)
        {
            return false;
        }
        wanted(&state, &members, &me, p, now)
    });
    keep_pending(&conversation, &list)?;
    // One that keeps failing, say an invitee with no keys published, mustn't
    // hold up the rest.
    let mut failed = None;
    for p in list {
        if matches!(p.change, GroupChange::Leave { .. }) {
            member_requests::leave(conversation)?;
            continue;
        }
        if let Err(e) = carry_or_ask(conversation, &me, p.change).await {
            failed = Some(e);
        }
    }
    failed.map_or(Ok(()), Err)
}

async fn carry_or_ask(conversation: [u8; 16], me: &[u8; 32], change: GroupChange) -> Result<()> {
    let mut state = require_state(&conversation)?;
    let change = match change {
        GroupChange::Leave { .. } => {
            // A committer can't commit its own removal: pass the role on.
            if state.committer == *me {
                let members = Conversation::active_members(&conversation);
                let to = successor(&state, &members, me)
                    .ok_or_else(|| anyhow!("nobody to hand the group to"))?;
                commit_change(
                    conversation,
                    sign(&conversation, GroupChange::Handover { to: to.into() })?,
                )
                .await?;
                state = require_state(&conversation)?;
            }
            leave_change(&state, me)
        },
        change => change,
    };
    let special_request =
        matches!(&change, GroupChange::MemberRequest(r) if r.who.0 == state.committer);
    if state.committer == *me || special_request {
        commit_change(conversation, sign(&conversation, change)?).await
    } else {
        send_change(conversation, change, state.committer).await
    }
}

fn wanted(state: &GroupState, members: &[[u8; 32]], me: &[u8; 32], p: &Pending, now: u64) -> bool {
    let done = match &p.change {
        GroupChange::Add { who } => who.iter().all(|w| members.contains(&w.0)),
        GroupChange::Remove { who } => !members.contains(&who.0),
        GroupChange::Leave { .. } => return members.contains(me),
        GroupChange::Role { who, role } => !members.contains(&who.0) || state.role(&who.0) == *role,
        GroupChange::Rules(rules) => state.rules == *rules,
        GroupChange::MemberRequest(request) => !members.contains(&request.who.0),
        _ => true,
    };
    !done
        && now.saturating_sub(p.since) < REQUEST_TTL_SECS
        && permitted(state, members, me, &p.change).is_ok()
}

/// Who takes the committer's role when it leaves: another owner, else an
/// admin, else whoever has been in the group longest.
fn successor(state: &GroupState, members: &[[u8; 32]], me: &[u8; 32]) -> Option<[u8; 32]> {
    let others = || members.iter().filter(|m| *m != me);
    state
        .owners
        .iter()
        .chain(&state.admins)
        .find(|m| *m != me && members.contains(m))
        .or_else(|| others().next())
        .copied()
}

/// Our leave, naming who owns the group after us when we're its last owner.
fn leave_change(state: &GroupState, me: &[u8; 32]) -> GroupChange {
    let successor = (state.owners == [*me]).then(|| state.committer.into());
    GroupChange::Leave { successor }
}

fn sign(conversation: &[u8; 16], change: GroupChange) -> Result<SignedChange> {
    let (me, signer) = local_signer()?;
    let group_id = require_group(conversation)?;
    let group = load_group(&PromtuzMlsProvider::shared(), &group_id)?;
    let epoch = group.epoch();
    let branch = group.branch_id();
    let sig = signer.sign(&group_change_signing_input(&group_id, epoch, &branch, &me, &change)?);
    Ok(SignedChange {
        by: me.into(),
        epoch,
        branch: branch.into(),
        change,
        sig: Bytes(sig.to_bytes()),
    })
}

/// Ask the committer, signed for the current epoch. Outboxed, and it wakes
/// their phone.
async fn send_change(
    conversation: [u8; 16], change: GroupChange, committer: [u8; 32],
) -> Result<()> {
    let signed = sign(&conversation, change)?;
    let request = AppPayload::GroupRequest(GroupRequest::Change(signed));
    crate::messaging::send_control_wake_to(conversation, request, committer).await
}

/// Introduce ourselves to a member who joined after us, and give them the
/// group's name when its state doesn't carry the current one.
async fn catch_up(conversation: [u8; 16], who: [u8; 32]) {
    if !Conversation::active_members(&conversation).contains(&who) {
        return;
    }
    let title = Conversation::get(&conversation).map(|c| c.title).unwrap_or_default();
    if !Conversation::has_signed_rules(&conversation) && !title.is_empty() {
        let titled = AppPayload::System(SystemEvent::Titled { title });
        if let Err(e) = crate::messaging::send_control_to(conversation, titled, who).await {
            warn!("GROUP: the new member may not have the group's name yet: {e}");
        }
    }
    crate::messaging::welcome::introduce_ourselves_to(conversation, who);
}

/// The committer's Welcome for someone we asked to add. They know us, and may
/// not know the committer, so we seal it as ours and deliver it.
pub(crate) fn forward_welcome(
    conversation: [u8; 16], _from: [u8; 32], who: [u8; 32], kp_ref: [u8; 32], welcome: Vec<u8>,
    history: Option<Vec<u8>>,
) -> Result<()> {
    let asked = asked_key(&conversation, &who);
    // The MLS application gate checked the sender's role at its own epoch.
    // We only vouch for invitations we actually requested.
    if crate::data::app_prefs::get(&asked).is_none() {
        return Ok(());
    }
    let (me, signer) = local_signer()?;
    let gid = require_group(&conversation)?;
    let env = crate::mls::seal_welcome_blob(welcome, gid, me, who, kp_ref, &signer)?;
    let envelope = match history {
        Some(history) => recovery::welcome_envelope(env, &history, &signer)?,
        None => MlsEnvelopeP::Welcome(env),
    };
    let mut copies = vec![welcome_copy(who, &envelope, &me, &signer)?];
    crate::delivery::enqueue_batch(&mut copies)?;
    crate::data::app_prefs::remove(&asked)?;
    core().spawn(async move {
        recovery::dispatch(copies).await;
    });
    Ok(())
}

fn asked_key(conversation: &[u8; 16], who: &[u8; 32]) -> String {
    format!("group_add:{}{}", hex::encode(conversation), hex::encode(who))
}

/// Tell the founder of a group from before signed rules that we can follow
/// them. Once per epoch, so one lost to a concurrent commit is said again.
async fn announce_ready(conversation: [u8; 16]) -> Result<()> {
    let me = local_ipk()?;
    let Some(state) = Conversation::state(&conversation) else { return Ok(()) };
    let founder = state.committer;
    let members = Conversation::active_members(&conversation);
    if founder == me || !members.contains(&me) || !members.contains(&founder) {
        return Ok(());
    }
    let group_id = require_group(&conversation)?;
    let epoch = load_group(&PromtuzMlsProvider::shared(), &group_id)?.epoch().to_string();
    let key = format!("group_recovery_ready_sent:{}", hex::encode(conversation));
    if crate::data::app_prefs::get(&key).as_deref() == Some(epoch.as_str()) {
        return Ok(());
    }
    crate::messaging::send_control_to(
        conversation,
        AppPayload::GroupRequest(GroupRequest::RecoveryReady),
        founder,
    )
    .await?;
    crate::data::app_prefs::set(&key, &epoch)
}

fn ready_key(conversation: &[u8; 16], who: &[u8; 32]) -> String {
    format!("group_recovery_ready:{}{}", hex::encode(conversation), hex::encode(who))
}

async fn ready_from(conversation: [u8; 16], from: [u8; 32]) -> Result<()> {
    if Conversation::has_signed_rules(&conversation)
        || !Conversation::active_members(&conversation).contains(&from)
    {
        return Ok(());
    }
    crate::data::app_prefs::set(&ready_key(&conversation, &from), "1")?;
    maybe_upgrade(conversation).await
}

/// Converts a group we founded before signed rules once every member can follow them: older
/// clients refuse the converting commit and would be left behind.
async fn maybe_upgrade(conversation: [u8; 16]) -> Result<()> {
    let me = local_ipk()?;
    if Conversation::has_signed_rules(&conversation) || !Conversation::is_owner(&conversation, &me)
    {
        return Ok(());
    }
    let members = Conversation::active_members(&conversation);
    let ready = |m: &[u8; 32]| {
        *m == me || crate::data::app_prefs::get(&ready_key(&conversation, m)).is_some()
    };
    if !members.iter().all(ready) {
        return Ok(());
    }
    commit_change(conversation, sign(&conversation, GroupChange::Upgrade)?).await?;
    for m in members {
        let _ = crate::data::app_prefs::remove(&ready_key(&conversation, &m));
    }
    info!("GROUP: {} now runs by signed rules", hex::encode(&conversation[..4]));
    Ok(())
}

/// Add someone to a group from before signed rules: its founder commits it and
/// Welcomes them directly.
async fn legacy_add(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    let _one = core().groups.membership.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_owner(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;
    if Conversation::active_members(&conversation).contains(&who) {
        bail!("that member is already in this group");
    }

    with_mls!(ctx, {
        let (kp, kp_ref) = crate::messaging::session::fetch_verified_keypackage(&ctx, &who, true)
            .await
            .map_err(|e| no_keys_error(&who, e))?;
        let mut group = load_settled(ctx.provider, &conversation, &group_id)?;
        if group.member_count() + 1 > crate::mls::MAX_GROUP_MEMBERS {
            bail!("a group is limited to {} members", crate::mls::MAX_GROUP_MEMBERS);
        }
        let (commit, welcome) = group
            .add_members(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?, &[kp])
            .map_err(|e| anyhow!("add_members: {e}"))?;
        let env =
            crate::mls::make_welcome_envelope(welcome, group_id, our_ipk, who, kp_ref, &ipk_signer)
                .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
        // Queued with the commit, so the joiner never holds an epoch the members never reach.
        let welcome = welcome_copy(who, &MlsEnvelopeP::Welcome(env), &our_ipk, &ipk_signer)?;
        let recipients = Conversation::recipients(&conversation);
        let copies = publish_commit(
            ctx.provider,
            &mut group,
            &recipients,
            &commit,
            &ipk_signer,
            Some(welcome),
        )?;
        Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
        recovery::dispatch(copies).await;
        announce(conversation, SystemEvent::Added { who: who.into() }).await;
        catch_up(conversation, who).await;
        Ok(())
    })
}

/// Only the founder's commit takes a leaf out of a pre-rules group. Committed inline rather than
/// by reference to the proposal, so a member who never saw it can still apply the commit.
pub async fn carry_leave(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    evict(conversation, who, None).await?;
    maybe_upgrade(conversation).await
}

async fn evict(
    conversation: [u8; 16], who: [u8; 32], narration: Option<SystemEvent>,
) -> Result<()> {
    let _one = core().groups.membership.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_owner(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;
    if who == our_ipk {
        bail!("use leave to remove yourself");
    }

    with_mls!(ctx, {
        let mut group = load_settled(ctx.provider, &conversation, &group_id)?;
        let idx = group
            .member_index_by_ipk(&who)
            .ok_or_else(|| anyhow!("that member is not in this group"))?;

        // Address the Commit to the roster as it stands now, the removed member included: they
        // need it to learn they are out.
        let recipients = Conversation::recipients(&conversation);
        let commit = group
            .remove_members(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?, &[idx])
            .map_err(|e| anyhow!("remove_members: {e}"))?;
        let copies =
            publish_commit(ctx.provider, &mut group, &recipients, &commit, &ipk_signer, None)?;

        // The tree, not our intent, is the roster: the commit may have
        // carried more than this one removal.
        Conversation::sync_group(&conversation, &group.roster(), group.group_meta().as_ref())?;
        recovery::dispatch(copies).await;
        if let Some(event) = narration {
            announce(conversation, event).await;
        }
        Ok(())
    })
}

/// Leave a group from before signed rules: propose our own removal, tell
/// everyone, then drop the local group state. Its founder carries the leave.
async fn legacy_leave(conversation: [u8; 16]) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;
    require_not_stranding_the_group(&conversation, &our_ipk)?;

    with_mls!(ctx, {
        let mut group = load_settled(ctx.provider, &conversation, &group_id)?;
        let recipients = Conversation::recipients(&conversation);
        let commit_epoch = group.epoch();

        // Announce first: once the group state is gone we can no longer encrypt to it.
        announce(conversation, SystemEvent::Left { who: our_ipk.into() }).await;

        let proposal = group
            .leave(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?)
            .map_err(|e| anyhow!("leave: {e}"))?;
        let mut copies =
            sealed_copies(&recipients, &proposal, group_id, commit_epoch, &ipk_signer)?;
        crate::delivery::enqueue_batch(&mut copies)?;
        recovery::dispatch(copies).await;

        Conversation::deactivate_member(&conversation, &our_ipk)?;
        if let Err(e) = group.delete(ctx.provider) {
            warn!("GROUP: dropping local group state after leave failed: {e}");
        }
        info!("GROUP: left {}", hex::encode(&conversation[..4]));
        Ok(())
    })
}

/// A sealed copy of `message` per recipient, so an offline member still applies it.
fn sealed_copies(
    recipients: &[[u8; 32]], message: &openmls::prelude::MlsMessageOut, group_id: [u8; 32],
    epoch: u64, ipk_signer: &SigningKey,
) -> Result<Vec<recovery::Copy>> {
    let sealed = SealedMessage::from_mls_out(message, group_id, epoch)
        .map_err(|e| anyhow!("seal commit: {e}"))?;
    Ok(recovery::copies(&recovery::addressed(
        &sealed,
        crate::data::message::next_dispatch_id(),
        recipients,
        ipk_signer,
        OpType::Control,
        common::proto::client_rel::Wake::Message,
        0,
    )?))
}

/// Queues our pending commit for every recipient, `welcome` beside it, with the mark that it left,
/// then merges it. Whatever fails here the next change settles. The caller sends the copies with
/// [`recovery::dispatch`].
fn publish_commit(
    provider: &PromtuzMlsProvider, group: &mut MlsGroupHandle, recipients: &[[u8; 32]],
    commit: &openmls::prelude::MlsMessageOut, ipk_signer: &SigningKey,
    welcome: Option<recovery::Copy>,
) -> Result<Vec<recovery::Copy>> {
    // Members apply it at the epoch it was built in, which the merge moves us past.
    let (group_id, epoch) = (group.group_id(), group.epoch());
    let mut copies = sealed_copies(recipients, commit, group_id, epoch, ipk_signer)?;
    copies.extend(welcome);
    crate::delivery::enqueue_commit(&mut copies, &group_id, epoch)?;
    group.merge_pending_commit(provider).map_err(|e| anyhow!("merge_pending_commit: {e}"))?;
    Ok(copies)
}

/// Settles the commit a failed or interrupted change left pending before the next is built: merged
/// if it was queued, since members hold it or will, and dropped otherwise.
fn load_settled(
    provider: &PromtuzMlsProvider, conversation: &[u8; 16], group_id: &[u8; 32],
) -> Result<MlsGroupHandle> {
    let mut group = load_group(provider, group_id)?;
    if !group.has_pending_commit() {
        return Ok(group);
    }
    if crate::delivery::commit_left(group_id, group.epoch())? {
        warn!("GROUP: merging a commit queued before an interruption");
        group.merge_pending_commit(provider)?;
        Conversation::sync_group(conversation, &group.roster(), group.group_meta().as_ref())?;
    } else {
        warn!("GROUP: dropping a commit that never left");
        group.clear_pending_commit(provider);
    }
    Ok(group)
}

/// `envelope` framed for `who`'s outbox row.
fn welcome_copy(
    who: [u8; 32], envelope: &MlsEnvelopeP, me: &[u8; 32], signer: &SigningKey,
) -> Result<recovery::Copy> {
    use common::proto::pack::Packer;
    let id = crate::data::message::next_dispatch_id();
    let wake = common::proto::client_rel::Wake::Message;
    let frame = crate::delivery::prepare_dispatch(&who, me, signer, &id, envelope.ser()?, wake, 0)?;
    Ok((who, id, OpType::Welcome, frame))
}

/// Drop our copy of a group we're out of. The conversation keeps its history.
fn forget_group(conversation: &[u8; 16]) {
    if let Some(gid) = Conversation::group_of(conversation) {
        crate::api::messaging::purge_mls_group(&gid);
        let _ = Conversation::unbind_group(conversation);
    }
}

/// Turns a KeyPackage miss into something a person can act on.
fn no_keys_error(who: &[u8; 32], e: anyhow::Error) -> anyhow::Error {
    let name = crate::data::contact::Contact::get(who)
        .map(|c| c.inner.name.clone())
        .unwrap_or_else(|| hex::encode(&who[..4]));
    if e.chain().any(|c| matches!(c.downcast_ref::<DhtClientError>(), Some(DhtClientError::NoStash)))
    {
        anyhow!("{name} hasn't published their keys yet — ask them to open the app once")
    } else {
        // root_cause, not the whole chain: the outer layers name our own call
        // sites, which tell the reader nothing they can act on.
        anyhow!("couldn't reach the network to fetch {name}'s keys ({})", e.root_cause())
    }
}

/// Refuses a departure that leaves nobody able to change the group: a pre-rules founder leaving
/// while others remain, or a committer deleting without leaving, which would hand nothing over.
pub(crate) fn require_not_stranding_the_group(
    conversation: &[u8; 16], who: &[u8; 32],
) -> Result<()> {
    let others = Conversation::active_members(conversation).iter().filter(|m| *m != who).count();
    if others == 0 || !Conversation::active_members(conversation).contains(who) {
        return Ok(());
    }
    if Conversation::has_signed_rules(conversation) {
        if Conversation::state(conversation).is_some_and(|s| s.committer == *who) {
            bail!("leave the group before deleting it, so someone else can manage it");
        }
        return Ok(());
    }
    if Conversation::is_owner(conversation, who) {
        bail!(
            "you created this group — remove the other {} member{} before leaving it",
            others,
            if others == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

fn require_owner(conversation: &[u8; 16], who: &[u8; 32]) -> Result<()> {
    if !Conversation::is_owner(conversation, who) {
        bail!("only the group's owner can make this change");
    }
    Ok(())
}

fn require_signed_rules(conversation: &[u8; 16]) -> Result<()> {
    require_group(conversation)?;
    if !Conversation::has_signed_rules(conversation) {
        bail!("this group needs every member on the latest version first");
    }
    Ok(())
}

fn require_state(conversation: &[u8; 16]) -> Result<GroupState> {
    Conversation::state(conversation).ok_or_else(|| anyhow!("no such group"))
}

fn require_group(conversation: &[u8; 16]) -> Result<[u8; 32]> {
    let row = Conversation::get(conversation).ok_or_else(|| anyhow!("no such conversation"))?;
    if row.kind != KIND_GROUP {
        bail!("membership changes only apply to group conversations");
    }
    Conversation::group_of(conversation).ok_or_else(|| anyhow!("this group has no MLS state"))
}

fn local_ipk() -> Result<[u8; 32]> {
    Ok(Identity::local_ipk().ok_or_else(|| anyhow!("identity not found"))?)
}

fn local_signer() -> Result<([u8; 32], SigningKey)> {
    let ipk = local_ipk()?;
    let signer = crate::data::identity::secret_key_signing(&ipk)?;
    Ok((ipk, signer))
}

fn load_group(provider: &PromtuzMlsProvider, group_id: &[u8; 32]) -> Result<MlsGroupHandle> {
    MlsGroupHandle::load(provider, group_id)
        .map_err(|e| anyhow!("load group: {e}"))?
        .ok_or_else(|| anyhow!("no local state for this group"))
}

fn leaf_for(
    provider: &PromtuzMlsProvider, group: &MlsGroupHandle, our_ipk: &[u8; 32],
) -> Result<openmls_basic_credential::SignatureKeyPair> {
    crate::messaging::session::leaf_signer_for_group(provider, group, our_ipk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ScopedCore;
    use crate::test_support::mls::Party;
    use crate::test_support::mls::commit_of;
    use crate::test_support::mls::found;
    use crate::test_support::mls::with_failing_trigger;

    /// A commit queued before its merge failed has reached the members, so the next change merges
    /// it; one that never reached the outbox is dropped. Either way the next change commits, and
    /// the members follow it.
    #[tokio::test]
    async fn the_next_change_settles_a_commit_left_pending() {
        let scope = ScopedCore::new();
        let [alice, bob, carol, dave] = [0xA7, 0xB7, 0xC7, 0xD7].map(Party::new);
        let gid = [0xE7; 32];
        let meta = GroupMeta { title: String::new(), founder: alice.ipk, state: None };
        let (mut group, [mut at_bob, _]) = found(&alice, gid, Some(&meta), [&bob, &carol]);
        let conversation =
            Conversation::join_group(&alice.ipk, &[alice.ipk, bob.ipk, carol.ipk]).unwrap();
        Conversation::bind_group(&conversation, &gid).unwrap();
        let provider = &alice.provider;
        let publish = |group: &mut MlsGroupHandle, commit| {
            publish_commit(provider, group, &[bob.ipk, carol.ipk], commit, &alice.identity, None)
        };
        let epoch = group.epoch();

        let carol_at = group.member_index_by_ipk(&carol.ipk).unwrap();
        let removal = group.remove_members(provider, &alice.leaf, &[carol_at]).unwrap();
        let merge = || publish(&mut group, &removal);
        assert!(with_failing_trigger(&alice.db, "INSERT ON mls_storage", merge).is_err());
        drop(group);
        let mut group = load_settled(provider, &conversation, &gid).unwrap();
        assert_eq!(group.epoch(), epoch + 1, "the queued removal is merged");
        let removal = commit_of(bob.receive(&mut at_bob, &removal));
        at_bob.merge_staged_commit(&bob.provider, removal).unwrap();

        let (add, _) = group.add_members(provider, &alice.leaf, &[dave.kp()]).unwrap();
        let queue = || publish(&mut group, &add);
        assert!(with_failing_trigger(scope.core.db.outbox(), "INSERT ON outbox", queue).is_err());
        drop(group);
        let mut group = load_settled(provider, &conversation, &gid).unwrap();
        assert_eq!(group.epoch(), epoch + 1, "the add that never left is dropped");

        let (add, welcome) = group.add_members(provider, &alice.leaf, &[dave.kp()]).unwrap();
        publish(&mut group, &add).unwrap();
        let add = commit_of(bob.receive(&mut at_bob, &add));
        at_bob.merge_staged_commit(&bob.provider, add).unwrap();
        let at_dave = dave.join(&welcome);
        assert_eq!(group.epoch(), epoch + 2);
        for member in [&at_bob, &at_dave] {
            assert_eq!((member.epoch(), member.roster()), (group.epoch(), group.roster()));
        }
    }
}
