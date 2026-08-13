//! Group membership — creating a group, adding, removing, leaving.
//!
//! Every membership change is one MLS Commit that has to reach *every existing
//! member*, plus a Welcome for anyone joining. Both ride the ordinary
//! Application envelope: openmls self-describes what an inner message is, and
//! the receive path already routes a `StagedCommitMessage` into
//! `merge_staged_commit`. That is why no new wire variant appears here.
//!
//! The ordering rule throughout: **fan the Commit out before merging it
//! locally**. Merge first and a failed fan-out leaves us an epoch ahead of
//! everyone, able to encrypt messages nobody can read.
//!
//! Authority (v1): the creator is the sole admin and may add or remove; any
//! member may leave. That is a policy check here, not a protocol rule, so
//! loosening it later needs no wire change.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::SystemEvent;
use ed25519_dalek::SigningKey;
use log::info;
use log::warn;

use crate::data::conversation::Conversation;
use crate::data::conversation::KIND_GROUP;
use crate::data::identity::Identity;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::db::mls::stash_db_handle;
use crate::db::outbox::OpType;
use crate::messaging::MlsContext;
use crate::messaging::SealedMessage;
use crate::quic::dht_client::DhtClient;
use crate::quic::dht_client::DhtClientError;
use crate::state::RELAY;

/// Bind `$ctx` to a live [`MlsContext`] for the body, or bail if no relay is
/// attached. Every membership change needs the network — a KeyPackage fetch
/// and a Welcome — so unlike a message there is no offline path to fall back
/// to. Expanded in place rather than wrapped in a closure so the borrows of
/// the provider/stash/buffer stay on the caller's stack.
macro_rules! with_mls {
    ($ctx:ident, $body:block) => {{
        let dht_client = {
            let guard = RELAY.read();
            guard.as_ref().and_then(|r| r.dht_client.clone())
        };
        let Some(client) = dht_client else {
            bail!("not connected to a relay; reconnect before changing group membership");
        };
        let provider = PromtuzMlsProvider::shared();
        let stash = KeyPackageStash::new(stash_db_handle());
        let buffer = EpochCatchupBuffer::new(stash_db_handle());
        let $ctx = MlsContext {
            provider: &provider,
            stash:    &stash,
            buffer:   &buffer,
            dht:      client.as_ref(),
        };
        $body
    }};
}

/// Create a group with `members` and us as the founding admin.
///
/// Returns the conversation id immediately usable for sending — the MLS group
/// exists and every member has been Welcomed by the time this returns.
pub async fn create_group(title: String, members: Vec<[u8; 32]>) -> Result<[u8; 16]> {
    if members.is_empty() {
        bail!("a group needs at least one other member");
    }
    if members.len() + 1 > crate::mls::MAX_GROUP_MEMBERS {
        bail!("a group is limited to {} members", crate::mls::MAX_GROUP_MEMBERS);
    }
    let our_ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
    if members.contains(&our_ipk) {
        bail!("you are already in the group you are creating");
    }
    let ipk_signer = crate::data::identity::secret_key_signing(&our_ipk)?;

    with_mls!(ctx, {
        // Every member's KeyPackage first: a member who has never published one
        // can't be added, and finding that out after minting the group would
        // leave a half-built group behind.
        let mut kps = Vec::with_capacity(members.len());
        for m in &members {
            let (kp, kp_ref) = crate::messaging::fetch_verified_keypackage(&ctx, m)
                .await
                .map_err(|e| no_keys_error(m, e))?;
            kps.push((*m, kp, kp_ref));
        }

        let group_id = crate::messaging::mint_group_id(&our_ipk);
        let (leaf_kp, _cwk) = crate::messaging::build_self_credential(&our_ipk)
            .map_err(|e| anyhow!("build credential: {e}"))?;
        leaf_kp.store(ctx.provider.storage()).map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

        let mut group = MlsGroupHandle::create(
            ctx.provider,
            &leaf_kp,
            &our_ipk,
            leaf_kp.public(),
            &group_id,
        )
        .map_err(|e| anyhow!("create group: {e}"))?;

        // One Commit adds everyone, and one Welcome covers them all — each
        // joiner finds their own secret inside it.
        let (_commit, welcome) = group
            .add_members(ctx.provider, &leaf_kp, &kps.iter().map(|(_, kp, _)| kp.clone()).collect::<Vec<_>>())
            .map_err(|e| anyhow!("add_members: {e}"))?;

        for (member, _, kp_ref) in &kps {
            let env = crate::mls::make_welcome_envelope(
                welcome.clone(),
                group_id,
                our_ipk,
                *member,
                *kp_ref,
                &ipk_signer,
            )
            .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
            if let Err(e) = ctx.dht.deliver_welcome(&env).await {
                // Roll the whole group back: a partially-Welcomed group is a
                // chat where some members can never decrypt anything.
                if let Err(de) = group.delete(ctx.provider) {
                    warn!("GROUP: rollback after welcome failure also failed: {de}");
                }
                return Err(anyhow!("deliver_welcome to a founding member: {e}"));
            }
        }

        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

        let conversation = Conversation::create_group(&title, &members)?;
        Conversation::bind_group(&conversation, &group_id)?;
        info!("GROUP: created \"{title}\" with {} members", members.len() + 1);

        // Tell them what it's called. The Welcome carries the group but not its
        // name, and this rides a different channel so it can lose the race —
        // hence the roster-derived fallback on the other side, and no local
        // system row for a name nobody has seen change.
        if !title.is_empty() {
            if let Err(e) = crate::messaging::send_control(
                conversation,
                AppPayload::System(SystemEvent::Titled { title }),
            )
            .await
            {
                warn!("GROUP: new members may not have the group's name yet: {e}");
            }
        }
        Ok(conversation)
    })
}

/// Add `who` to an existing group: Commit to the current members, Welcome to
/// the joiner. They get no pre-join history — MLS forward secrecy means the
/// keys for it no longer exist.
pub async fn add_member(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    require_admin(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;

    if Conversation::members(&conversation).iter().any(|m| m.active && m.member_ipk == who) {
        bail!("that member is already in this group");
    }

    with_mls!(ctx, {
        let (kp, kp_ref) = crate::messaging::fetch_verified_keypackage(&ctx, &who)
            .await
            .map_err(|e| no_keys_error(&who, e))?;
        let mut group = load_group(ctx.provider, &group_id)?;

        if group.member_count() + 1 > crate::mls::MAX_GROUP_MEMBERS {
            bail!("a group is limited to {} members", crate::mls::MAX_GROUP_MEMBERS);
        }
        // Existing members apply this Commit at the epoch it was built in, so
        // capture that before the merge moves us on.
        let commit_epoch = group.epoch();
        let (commit, welcome) =
            group.add_members(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?, &[kp])
                .map_err(|e| anyhow!("add_members: {e}"))?;

        let env = crate::mls::make_welcome_envelope(
            welcome, group_id, our_ipk, who, kp_ref, &ipk_signer,
        )
        .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
        ctx.dht.deliver_welcome(&env).await.map_err(|e| anyhow!("deliver_welcome: {e}"))?;

        fan_out_commit(&conversation, &commit, group_id, commit_epoch, &our_ipk, &ipk_signer)
            .await?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

        Conversation::add_member(&conversation, &who, crate::data::conversation::ROLE_MEMBER)?;
        crate::messaging::announce(
            conversation,
            SystemEvent::Added { who: who.into() },
        )
        .await;

        // The joiner alone needs the name — everyone else already has it, and
        // broadcasting would draw "X named the group" in every member's chat
        // for a rename that never happened.
        let title = Conversation::get(&conversation).map(|c| c.title).unwrap_or_default();
        if !title.is_empty() {
            if let Err(e) = crate::messaging::send_control_to(
                conversation,
                AppPayload::System(SystemEvent::Titled { title }),
                who,
            )
            .await
            {
                warn!("GROUP: the new member may not have the group's name yet: {e}");
            }
        }
        Ok(())
    })
}

/// Remove `who`, then rotate our own leaf key so the removed device cannot
/// read anything sent afterwards even if it kept the old epoch's secrets.
pub async fn remove_member(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    require_admin(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;
    if who == our_ipk {
        bail!("use leave to remove yourself");
    }

    with_mls!(ctx, {
        let mut group = load_group(ctx.provider, &group_id)?;
        let idx = group
            .member_index_by_ipk(&who)
            .ok_or_else(|| anyhow!("that member is not in this group"))?;

        // Address the Commit to the roster as it stands *now*, the removed
        // member included: they need it to learn they're out, and everyone
        // else needs it to converge.
        let recipients = Conversation::recipients(&conversation);
        let commit_epoch = group.epoch();
        let commit = group
            .remove_members(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?, &[idx])
            .map_err(|e| anyhow!("remove_members: {e}"))?;
        fan_out_commit_to(&recipients, &commit, group_id, commit_epoch, &our_ipk, &ipk_signer)
            .await?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

        Conversation::deactivate_member(&conversation, &who)?;

        // Post-compromise security: a removal is exactly the moment to assume
        // the departing device's key material is untrusted, so rotate ours.
        let rotate_epoch = group.epoch();
        let update = group
            .self_update(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?)
            .map_err(|e| anyhow!("self_update: {e}"))?;
        let remaining = Conversation::recipients(&conversation);
        fan_out_commit_to(&remaining, &update, group_id, rotate_epoch, &our_ipk, &ipk_signer)
            .await?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit after self_update: {e}"))?;

        crate::messaging::announce(
            conversation,
            SystemEvent::Removed { who: who.into() },
        )
        .await;
        Ok(())
    })
}

/// Leave a group: propose our own removal, tell everyone, then drop the local
/// group state. The conversation and its history stay — leaving a chat is not
/// deleting it.
pub async fn leave(conversation: [u8; 16]) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;

    with_mls!(ctx, {
        let mut group = load_group(ctx.provider, &group_id)?;
        let recipients = Conversation::recipients(&conversation);
        let commit_epoch = group.epoch();

        // Announce before tearing anything down — once the group state is gone
        // we can no longer encrypt to it.
        crate::messaging::announce(
            conversation,
            SystemEvent::Left { who: our_ipk.into() },
        )
        .await;

        let proposal = group
            .leave(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?)
            .map_err(|e| anyhow!("leave: {e}"))?;
        fan_out_commit_to(&recipients, &proposal, group_id, commit_epoch, &our_ipk, &ipk_signer)
            .await?;

        Conversation::deactivate_member(&conversation, &our_ipk)?;
        if let Err(e) = group.delete(ctx.provider) {
            warn!("GROUP: dropping local group state after leave failed: {e}");
        }
        // The conversation keeps its history but can no longer send.
        info!("GROUP: left {}", hex::encode(&conversation[..4]));
        Ok(())
    })
}

/// Fan a Commit out to every current member of `conversation`.
async fn fan_out_commit(
    conversation: &[u8; 16], commit: &openmls::prelude::MlsMessageOut, group_id: [u8; 32],
    epoch: u64, our_ipk: &[u8; 32], ipk_signer: &SigningKey,
) -> Result<()> {
    let recipients = Conversation::recipients(conversation);
    fan_out_commit_to(&recipients, commit, group_id, epoch, our_ipk, ipk_signer).await
}

/// Fan a Commit out to an explicit recipient list — used where the roster is
/// mid-change and "current members" would be the wrong set.
///
/// Outboxed as Control, so a member who is offline still applies the
/// membership change on their next reconnect rather than silently forking off
/// the group.
async fn fan_out_commit_to(
    recipients: &[[u8; 32]], commit: &openmls::prelude::MlsMessageOut, group_id: [u8; 32],
    epoch: u64, our_ipk: &[u8; 32], ipk_signer: &SigningKey,
) -> Result<()> {
    let sealed = SealedMessage::from_mls_out(commit, group_id, epoch)
        .map_err(|e| anyhow!("seal commit: {e}"))?;
    let id = crate::data::message::next_dispatch_id();
    for to in recipients {
        let env = sealed
            .address_to(to, ipk_signer)
            .map_err(|e| anyhow!("address commit to member: {e}"))?;
        crate::messaging::dispatch_to_member(
            to,
            our_ipk,
            ipk_signer,
            &id,
            env,
            OpType::Control,
            true,
        )
        .await;
    }
    Ok(())
}

/// Turn a KeyPackage miss into something a person can act on.
///
/// This is *the* common failure when adding someone: their keys are published
/// by their own device, so one that has never been online since pairing has
/// nothing for us to fetch. "fetch_keypackage_for" tells the user nothing;
/// what to do about it does.
fn no_keys_error(who: &[u8; 32], e: anyhow::Error) -> anyhow::Error {
    // Their name if we hold one — the message is read by someone who thinks in
    // names, not keys; the hex head is only a fallback for a stranger.
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

/// v1 authority: only the creator adds and removes.
fn require_admin(conversation: &[u8; 16], who: &[u8; 32]) -> Result<()> {
    if !Conversation::is_admin(conversation, who) {
        bail!("only the group's creator can change its membership");
    }
    Ok(())
}

fn require_group(conversation: &[u8; 16]) -> Result<[u8; 32]> {
    let row = Conversation::get(conversation).ok_or_else(|| anyhow!("no such conversation"))?;
    if row.kind != KIND_GROUP {
        bail!("membership changes only apply to group conversations");
    }
    Conversation::group_of(conversation).ok_or_else(|| anyhow!("this group has no MLS state"))
}

fn local_signer() -> Result<([u8; 32], SigningKey)> {
    let ipk = Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk();
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
    crate::messaging::leaf_signer_for_group(provider, group, our_ipk)
}
