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

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::SystemEvent;
use ed25519_dalek::SigningKey;
use log::info;
use log::warn;
use once_cell::sync::Lazy;
use parking_lot::Mutex as PlMutex;
use tokio::sync::Mutex as TokMutex;

use crate::data::conversation::Conversation;
use crate::data::conversation::KIND_GROUP;
use crate::data::identity::Identity;
use crate::mls::EpochCatchupBuffer;
use crate::mls::KeyPackageStash;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::db::mls::stash_db_handle;
use crate::db::outbox::OpType;
use crate::delivery;
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

/// One membership change per conversation at a time.
///
/// Every FFI call spawns its own task and an inbound leave spawns
/// [`carry_leave`], so without this two changes could interleave: one awaiting
/// a Welcome delivery with its commit pending while the other loads the group,
/// takes that commit for a leftover, and builds its own over it. Same shape as
/// the group-creation lock in `messaging`: the outer mutex guards the map and
/// is never held across an await; the inner one is what a change holds for its
/// whole run, network round trips included. Entries are never removed; each is
/// a few words, one per group changed this session.
#[allow(clippy::type_complexity)]
static MEMBERSHIP_LOCKS: Lazy<PlMutex<HashMap<[u8; 16], Arc<TokMutex<()>>>>> =
    Lazy::new(|| PlMutex::new(HashMap::new()));

fn membership_lock(conversation: &[u8; 16]) -> Arc<TokMutex<()>> {
    MEMBERSHIP_LOCKS
        .lock()
        .entry(*conversation)
        .or_insert_with(|| Arc::new(TokMutex::new(())))
        .clone()
}

/// What to do with a commit found still pending as a change begins.
///
/// It belongs to a change that died before merging, and whether members ever
/// saw it decides everything. One that left the device (its outbox rows were
/// written, so members hold it or will) has to be merged, or founder and
/// members part ways at the next epoch; one that never left has to be dropped,
/// or openmls refuses every change over it. The mark written with the rows is
/// what tells the two apart, keyed on the epoch the commit was built at, which
/// is where a group with a pending commit still stands.
///
/// A merged recovery re-reads the roster from the tree. The system event the
/// original change would have announced is lost with it: a missing line of
/// narration, not a missing member.
fn recover(
    group: &mut MlsGroupHandle, provider: &PromtuzMlsProvider, conversation: &[u8; 16],
) -> Result<()> {
    if !group.has_pending_commit() {
        return Ok(());
    }
    let gid = group.group_id();
    if delivery::commit_left(&gid, group.epoch()) {
        warn!("GROUP: merging a commit that left before it was merged ({})", hex::encode(&gid[..4]));
        group
            .merge_pending_commit(provider)
            .map_err(|e| anyhow!("merge recovered commit: {e}"))?;
        delivery::commit_settled(&gid);
        Conversation::sync_roster(conversation, &group.roster())?;
    } else {
        warn!("GROUP: dropping a commit that never left ({})", hex::encode(&gid[..4]));
        group
            .clear_pending_commit(provider)
            .map_err(|e| anyhow!("drop unmerged commit: {e}"))?;
    }
    Ok(())
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
            let (kp, kp_ref) = crate::messaging::fetch_verified_keypackage(&ctx, m, true)
                .await
                .map_err(|e| no_keys_error(m, e))?;
            kps.push((*m, kp, kp_ref));
        }

        let group_id = crate::messaging::mint_group_id(&our_ipk);
        let (leaf_kp, cwk) = crate::messaging::build_self_credential(&ipk_signer)
            .map_err(|e| anyhow!("build credential: {e}"))?;
        leaf_kp.store(ctx.provider.storage()).map_err(|e| anyhow!("store leaf kp: {e:?}"))?;

        // The meta is what tells every joiner this is a group and not a pair.
        // It rides in the MLS group context, so it arrives inside the Welcome
        // and no relay can strip it.
        let meta = crate::mls::GroupMeta { title: title.clone(), founder: our_ipk };
        let mut group = MlsGroupHandle::create(ctx.provider, &leaf_kp, cwk, &group_id, Some(&meta))
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
        // Members who paired with us know our name; members who only ever
        // *received* an add from us may not. Cheap to say either way.
        crate::messaging::introduce_ourselves(conversation);
        Ok(conversation)
    })
}

/// Add `who` to an existing group: Commit to the current members, Welcome to
/// the joiner. They get no pre-join history — MLS forward secrecy means the
/// keys for it no longer exist.
pub async fn add_member(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    let lock = membership_lock(&conversation);
    let _one_at_a_time = lock.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_admin(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;

    if Conversation::members(&conversation).iter().any(|m| m.active && m.member_ipk == who) {
        bail!("that member is already in this group");
    }

    with_mls!(ctx, {
        let (kp, kp_ref) = crate::messaging::fetch_verified_keypackage(&ctx, &who, true)
            .await
            .map_err(|e| no_keys_error(&who, e))?;
        let mut group = load_group(ctx.provider, &group_id)?;
        recover(&mut group, ctx.provider, &conversation)?;

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
        .map_err(|e| abandon(&mut group, ctx.provider, anyhow!("make_welcome_envelope: {e}")))?;
        ctx.dht
            .deliver_welcome(&env)
            .await
            .map_err(|e| abandon(&mut group, ctx.provider, anyhow!("deliver_welcome: {e}")))?;

        fan_out_commit(&conversation, &commit, group_id, commit_epoch, &our_ipk, &ipk_signer)
            .await
            .map_err(|e| abandon(&mut group, ctx.provider, e))?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;
        delivery::commit_settled(&group_id);

        Conversation::sync_roster(&conversation, &group.roster())?;
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
        crate::messaging::introduce_ourselves_to(conversation, who);
        Ok(())
    })
}

/// Remove `who`, then rotate our own leaf key so the removed device cannot
/// read anything sent afterwards even if it kept the old epoch's secrets.
pub async fn remove_member(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    evict(conversation, who, true).await
}

/// Carry a member's leave: they proposed their own removal and announced it,
/// and only the founder's commit takes a leaf out of the tree. Committed
/// inline rather than by reference to their proposal, so a member who never
/// saw the proposal can still apply the commit.
pub async fn carry_leave(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    evict(conversation, who, false).await
}

async fn evict(conversation: [u8; 16], who: [u8; 32], announce: bool) -> Result<()> {
    let lock = membership_lock(&conversation);
    let _one_at_a_time = lock.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_admin(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;
    if who == our_ipk {
        bail!("use leave to remove yourself");
    }

    with_mls!(ctx, {
        let mut group = load_group(ctx.provider, &group_id)?;
        recover(&mut group, ctx.provider, &conversation)?;
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
            .await
            .map_err(|e| abandon(&mut group, ctx.provider, e))?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;
        delivery::commit_settled(&group_id);

        // The tree, not our intent, is the roster: the commit may have
        // carried more than this one removal.
        Conversation::sync_roster(&conversation, &group.roster())?;

        // Post-compromise security: a removal is exactly the moment to assume
        // the departing device's key material is untrusted, so rotate ours.
        let rotate_epoch = group.epoch();
        let update = group
            .self_update(ctx.provider, &leaf_for(ctx.provider, &group, &our_ipk)?)
            .map_err(|e| anyhow!("self_update: {e}"))?;
        let remaining = Conversation::recipients(&conversation);
        fan_out_commit_to(&remaining, &update, group_id, rotate_epoch, &our_ipk, &ipk_signer)
            .await
            .map_err(|e| abandon(&mut group, ctx.provider, e))?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit after self_update: {e}"))?;
        delivery::commit_settled(&group_id);

        if announce {
            crate::messaging::announce(
                conversation,
                SystemEvent::Removed { who: who.into() },
            )
            .await;
        }
        Ok(())
    })
}

/// Leave a group: propose our own removal, tell everyone, then drop the local
/// group state. The conversation and its history stay — leaving a chat is not
/// deleting it.
pub async fn leave(conversation: [u8; 16]) -> Result<()> {
    let lock = membership_lock(&conversation);
    let _one_at_a_time = lock.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;
    require_not_stranding_the_group(&conversation, &our_ipk)?;

    with_mls!(ctx, {
        let mut group = load_group(ctx.provider, &group_id)?;
        recover(&mut group, ctx.provider, &conversation)?;
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
        fan_out_proposal_to(&recipients, &proposal, group_id, commit_epoch, &our_ipk, &ipk_signer)
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
    fan_out_to(recipients, commit, group_id, epoch, our_ipk, ipk_signer, true).await
}

/// A proposal rides the same path but leaves no mark: nothing is pending
/// behind it, so there is nothing a recovery could take it for.
async fn fan_out_proposal_to(
    recipients: &[[u8; 32]], proposal: &openmls::prelude::MlsMessageOut, group_id: [u8; 32],
    epoch: u64, our_ipk: &[u8; 32], ipk_signer: &SigningKey,
) -> Result<()> {
    fan_out_to(recipients, proposal, group_id, epoch, our_ipk, ipk_signer, false).await
}

/// Address every copy first, then queue them all in one write, for a commit
/// together with the mark that it left (see [`recover`]), then send. The order
/// is the point: a failure while addressing is still a change that never left,
/// and once the write lands the commit is out of our hands whatever the
/// network does, which is exactly what the mark records.
async fn fan_out_to(
    recipients: &[[u8; 32]], msg: &openmls::prelude::MlsMessageOut, group_id: [u8; 32],
    epoch: u64, our_ipk: &[u8; 32], ipk_signer: &SigningKey, is_commit: bool,
) -> Result<()> {
    let sealed = SealedMessage::from_mls_out(msg, group_id, epoch)
        .map_err(|e| anyhow!("seal: {e}"))?;
    let id = crate::data::message::next_dispatch_id();
    let mut copies = Vec::with_capacity(recipients.len());
    for to in recipients {
        let env = sealed
            .address_to(to, ipk_signer)
            .map_err(|e| anyhow!("address to member: {e}"))?;
        let framed = crate::messaging::frame_dispatch(to, our_ipk, ipk_signer, &id, env, true)
            .ok_or_else(|| anyhow!("frame dispatch to member"))?;
        copies.push((*to, framed));
    }
    if is_commit {
        delivery::enqueue_commit(&group_id, epoch, &id, &copies)?;
    } else {
        for (to, bytes) in &copies {
            delivery::enqueue(&id, OpType::Control, Some(*to), bytes);
        }
    }
    for (to, bytes) in &copies {
        crate::messaging::send_framed(to, &id, bytes).await;
    }
    Ok(())
}

/// The change that built `group`'s pending commit failed before it reached
/// anyone: drop the commit, so the group stays at the epoch the members share.
/// The builders would clear it on the next attempt anyway; clearing here keeps
/// the stored state honest in between. Returns `why` so it slots into `map_err`.
fn abandon(
    group: &mut MlsGroupHandle, provider: &PromtuzMlsProvider, why: anyhow::Error,
) -> anyhow::Error {
    if let Err(e) = group.clear_pending_commit(provider) {
        warn!("GROUP: could not drop an unmerged commit: {e}");
    }
    why
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

/// The creator may not walk out of a group other people are still in.
///
/// v1 mints exactly one admin, so their leaving would strand everyone else in
/// a group nobody can add to, remove from or rename. Enforced here rather than
/// only in the UI: the FFI is reachable without it, and the group this protects
/// belongs to the other members as much as to the caller.
///
/// The way out is to empty the group first; handing it to another member is the
/// obvious next step and needs a wire event of its own.
///
/// Only binds someone still in the group — [`Conversation::is_admin`] reads the
/// active roster, so a creator who is already out is nobody's admin and their
/// own copy is theirs alone to drop.
pub(crate) fn require_not_stranding_the_group(
    conversation: &[u8; 16], who: &[u8; 32],
) -> Result<()> {
    let conn = crate::db::messages::MESSAGES_DB.lock();
    require_not_stranding_the_group_tx(&conn, conversation, who)
}

fn require_not_stranding_the_group_tx(
    conn: &rusqlite::Connection, conversation: &[u8; 16], who: &[u8; 32],
) -> Result<()> {
    if !Conversation::is_admin_tx(conn, conversation, who) {
        return Ok(());
    }
    // Everyone but `who`: the duty is theirs, so they are the one excluded,
    // not whoever happens to be signed in.
    let others = Conversation::recipients_tx(conn, conversation, Some(*who)).len();
    if others > 0 {
        bail!(
            "you created this group — remove the other {} member{} before leaving it",
            others,
            if others == 1 { "" } else { "s" }
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;

    /// The founder's duty is to the group they are in. Once they are out — a
    /// commit dropped them, or a restore brought back a roster without them —
    /// there is nobody left for them to strand, and refusing to let them drop
    /// their own copy pins a chat to the home list that nothing can ever
    /// remove.
    #[test]
    fn a_founder_who_has_left_may_still_delete_their_copy() {
        let conn = open_in_memory();
        let me = [1u8; 32];
        let group =
            Conversation::join_group_tx(&conn, &me, &[me, [2u8; 32], [3u8; 32]]).expect("found");

        assert!(
            require_not_stranding_the_group_tx(&conn, &group, &me).is_err(),
            "while we are in it, walking out on two other people is refused"
        );

        Conversation::deactivate_member_tx(&conn, &group, &me).expect("leave");
        require_not_stranding_the_group_tx(&conn, &group, &me)
            .expect("out of the group, the group is no longer ours to strand");
    }
}
