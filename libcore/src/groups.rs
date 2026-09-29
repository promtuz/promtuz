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
//! Authority: the founder's device makes every membership commit, so every
//! member applies one order of changes and the group can't split. Receivers
//! enforce that in [`MlsGroupHandle::commit_is_permitted`]. Anyone may add
//! someone and admins may remove, by asking the founder's device when it isn't
//! theirs ([`GroupRequest`]). The founder appoints the other admins. Any member
//! may leave.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::proto::mls_wire::AppPayload;
use common::proto::mls_wire::GroupRequest;
use common::proto::mls_wire::SystemEvent;
use common::types::bytes::Bytes;
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

/// One membership commit at a time. Two built from the same epoch would fork
/// the group, and our own device is the one place that can happen.
static MEMBERSHIP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Add `who` to a group. They get no pre-join history: MLS forward secrecy
/// means the keys for it no longer exist. The founder's device commits it at
/// once; anyone else's asks the founder's device to, and returns `false`.
pub async fn add_member(conversation: [u8; 16], who: [u8; 32]) -> Result<bool> {
    let our_ipk = local_ipk()?;
    require_group(&conversation)?;
    if Conversation::is_owner(&conversation, &our_ipk) {
        add_as_owner(conversation, who, None).await?;
        return Ok(true);
    }
    if is_member(&conversation, &who) {
        bail!("that member is already in this group");
    }
    crate::data::app_prefs::set(&asked_key(&conversation, &who), "1")?;
    ask_owner(conversation, GroupRequest::Add { who: who.into() }).await?;
    Ok(false)
}

/// Commit to the current members, Welcome to the joiner. `by` is the member
/// who asked for the add, when it wasn't us.
async fn add_as_owner(conversation: [u8; 16], who: [u8; 32], by: Option<[u8; 32]>) -> Result<()> {
    let _one = MEMBERSHIP.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_owner(&conversation, &our_ipk)?;
    let group_id = require_group(&conversation)?;

    if is_member(&conversation, &who) {
        bail!("that member is already in this group");
    }

    with_mls!(ctx, {
        let (kp, kp_ref) = crate::messaging::fetch_verified_keypackage(&ctx, &who, true)
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

        // `who` may not know us, and refuses a group Welcome from a stranger,
        // so the member who asked delivers it instead. We can send to them
        // once the commit is merged.
        let forward = match by {
            None => {
                let env = crate::mls::make_welcome_envelope(
                    welcome, group_id, our_ipk, who, kp_ref, &ipk_signer,
                )
                .map_err(|e| anyhow!("make_welcome_envelope: {e}"))?;
                ctx.dht.deliver_welcome(&env).await.map_err(|e| anyhow!("deliver_welcome: {e}"))?;
                None
            },
            Some(by) => {
                let blob = crate::mls::encode_welcome(&welcome).map_err(|e| anyhow!("encode welcome: {e}"))?;
                Some((by, blob))
            },
        };

        fan_out_commit(&conversation, &commit, group_id, commit_epoch, &our_ipk, &ipk_signer)
            .await?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit: {e}"))?;

        Conversation::sync_roster(&conversation, &group.roster())?;
        let narration = match by {
            None => SystemEvent::Added { who: who.into() },
            Some(by) => SystemEvent::AddedBy { who: who.into(), by: by.into() },
        };
        crate::messaging::announce(conversation, narration).await;

        let Some((by, welcome)) = forward else {
            catch_up(conversation, who).await;
            return Ok(());
        };
        // They ask for the catch-up themselves once they're in: anything we sent
        // them before the Welcome reached them was dropped.
        let welcome = AppPayload::GroupWelcome { who: who.into(), kp_ref: kp_ref.into(), welcome };
        crate::messaging::send_control_wake_to(conversation, welcome, by).await
    })
}

/// Tell a new member the group's name, which the Welcome only carries as it was
/// founded, and introduce ourselves. The joiner alone needs the name: sent to
/// everyone, it would draw "X named the group" for a rename that never happened.
async fn catch_up(conversation: [u8; 16], who: [u8; 32]) {
    let title = Conversation::get(&conversation).map(|c| c.title).unwrap_or_default();
    if !title.is_empty() {
        let titled = AppPayload::System(SystemEvent::Titled { title });
        if let Err(e) = crate::messaging::send_control_to(conversation, titled, who).await {
            warn!("GROUP: the new member may not have the group's name yet: {e}");
        }
    }
    crate::messaging::introduce_ourselves_to(conversation, who);
}

/// The founder's Welcome for someone we asked to add. They know us, and may not
/// know the founder, so we seal it as ours and deliver it.
pub(crate) fn forward_welcome(
    conversation: [u8; 16], from: [u8; 32], who: [u8; 32], kp_ref: [u8; 32], welcome: Vec<u8>,
) {
    if !may_forward(&conversation, &from, &who) {
        return warn!("GROUP: ignored a Welcome to deliver from {}", hex::encode(&from[..4]));
    }
    let asked = asked_key(&conversation, &who);
    crate::RUNTIME.spawn(async move {
        for attempt in 1..=3 {
            match deliver_welcome(conversation, who, kp_ref, welcome.clone()).await {
                Ok(()) => {
                    let _ = crate::data::app_prefs::remove(&asked);
                    return;
                },
                Err(e) => warn!("GROUP: delivering a Welcome failed (attempt {attempt}): {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
}

async fn deliver_welcome(conversation: [u8; 16], who: [u8; 32], kp_ref: [u8; 32], welcome: Vec<u8>) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;
    let env = crate::mls::seal_welcome_blob(welcome, group_id, our_ipk, who, kp_ref, &ipk_signer)
        .map_err(|e| anyhow!("seal welcome: {e}"))?;
    let client = RELAY.read().as_ref().and_then(|r| r.dht_client.clone())
        .ok_or_else(|| anyhow!("not connected to a relay"))?;
    client.deliver_welcome(&env).await.map_err(|e| anyhow!("deliver_welcome: {e}"))
}

/// Only the founder's Welcome, and only for someone we asked them to add: we
/// vouch for it to a contact of ours.
fn may_forward(conversation: &[u8; 16], from: &[u8; 32], who: &[u8; 32]) -> bool {
    Conversation::is_owner(conversation, from)
        && crate::data::app_prefs::get(&asked_key(conversation, who)).is_some()
}

/// Marks an add we asked the founder for, so we deliver only the Welcomes we
/// wanted delivered.
fn asked_key(conversation: &[u8; 16], who: &[u8; 32]) -> String {
    format!("group_add:{}{}", hex::encode(conversation), hex::encode(who))
}

/// Remove `who`, then rotate our own leaf key so the removed device cannot
/// read anything sent afterwards even if it kept the old epoch's secrets.
/// Admins may; as with an add, only the founder's device commits it, and an
/// admin's call returns `false` once it has asked.
pub async fn remove_member(conversation: [u8; 16], who: [u8; 32]) -> Result<bool> {
    let our_ipk = local_ipk()?;
    require_group(&conversation)?;
    if Conversation::is_owner(&conversation, &our_ipk) {
        evict(conversation, who, Some(SystemEvent::Removed { who: who.into() })).await?;
        return Ok(true);
    }
    require_admin(&conversation, &our_ipk)?;
    if who == our_ipk {
        bail!("use leave to remove yourself");
    }
    if Conversation::owner(&conversation) == Some(who) {
        bail!("the group's owner can't be removed");
    }
    ask_owner(conversation, GroupRequest::Remove { who: who.into() }).await?;
    Ok(false)
}

/// A member asked us, the founder, for a change only our device may commit.
pub(crate) fn requested(conversation: [u8; 16], from: [u8; 32], request: GroupRequest) {
    let Some(me) = Identity::get().map(|i| i.ipk()) else { return };
    if !Conversation::is_owner(&conversation, &me) || !may_carry(&conversation, &from, &request) {
        return warn!("GROUP: refused a membership request from {}", hex::encode(&from[..4]));
    }
    crate::RUNTIME.spawn(async move {
        let done = match request {
            GroupRequest::Add { who } => add_as_owner(conversation, who.0, Some(from)).await,
            GroupRequest::Remove { who } => {
                evict(conversation, who.0, Some(SystemEvent::RemovedBy { who, by: from.into() })).await
            },
            GroupRequest::Sync => {
                catch_up(conversation, from).await;
                Ok(())
            },
        };
        if let Err(e) = done {
            warn!("GROUP: could not carry a request from {}: {e}", hex::encode(&from[..4]));
        }
    });
}

/// Anyone still in the group may ask to add someone, or to catch up. Only an
/// admin may ask to remove someone, and never the founder or themselves.
fn may_carry(conversation: &[u8; 16], from: &[u8; 32], request: &GroupRequest) -> bool {
    is_member(conversation, from)
        && match request {
            GroupRequest::Add { .. } | GroupRequest::Sync => true,
            GroupRequest::Remove { who } => {
                who.0 != *from
                    && Conversation::is_admin(conversation, from)
                    && !Conversation::is_owner(conversation, &who.0)
            },
        }
}

/// Make `who` an admin of a group we own, or stop them being one. Every
/// member hears the new list.
pub async fn set_admin(conversation: [u8; 16], who: [u8; 32], admin: bool) -> Result<()> {
    let our_ipk = local_ipk()?;
    require_group(&conversation)?;
    require_owner(&conversation, &our_ipk)?;
    if who == our_ipk {
        bail!("you're always an admin of a group you own");
    }
    if !is_member(&conversation, &who) {
        bail!("they're not in this group");
    }
    let mut admins = Conversation::admins(&conversation);
    admins.retain(|a| *a != who);
    if admin {
        admins.push(who);
    }
    let list = admins.iter().copied().map(Into::into).collect();
    crate::messaging::send_control(conversation, AppPayload::GroupAdmins { admins: list }).await?;
    Conversation::set_admins(&conversation, &admins)
}

/// The founder's list of admins. Nobody else's counts.
pub(crate) fn receive_admins(conversation: [u8; 16], from: [u8; 32], admins: Vec<Bytes<32>>) {
    if !Conversation::is_owner(&conversation, &from) {
        return warn!("GROUP: ignored an admin list from {}", hex::encode(&from[..4]));
    }
    let admins: Vec<[u8; 32]> = admins.into_iter().map(|a| a.0).collect();
    if let Err(e) = Conversation::set_admins(&conversation, &admins) {
        warn!("GROUP: could not apply the admin list: {e}");
    }
}

/// Carry a member's leave: they proposed their own removal and announced it,
/// and only the founder's commit takes a leaf out of the tree. Committed
/// inline rather than by reference to their proposal, so a member who never
/// saw the proposal can still apply the commit.
pub async fn carry_leave(conversation: [u8; 16], who: [u8; 32]) -> Result<()> {
    evict(conversation, who, None).await
}

async fn evict(conversation: [u8; 16], who: [u8; 32], narration: Option<SystemEvent>) -> Result<()> {
    let _one = MEMBERSHIP.lock().await;
    let (our_ipk, ipk_signer) = local_signer()?;
    require_owner(&conversation, &our_ipk)?;
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
            .await?;
        group
            .merge_pending_commit(ctx.provider)
            .map_err(|e| anyhow!("merge_pending_commit after self_update: {e}"))?;

        if let Some(event) = narration {
            crate::messaging::announce(conversation, event).await;
        }
        Ok(())
    })
}

/// Leave a group: propose our own removal, tell everyone, then drop the local
/// group state. The conversation and its history stay — leaving a chat is not
/// deleting it.
pub async fn leave(conversation: [u8; 16]) -> Result<()> {
    let (our_ipk, ipk_signer) = local_signer()?;
    let group_id = require_group(&conversation)?;
    require_not_stranding_the_group(&conversation, &our_ipk)?;

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
            common::proto::client_rel::Wake::Message,
            0,
        )
        .await;
    }
    Ok(())
}

/// Ask the founder's device for a change only it may commit. Outboxed, and it
/// wakes their device, so an owner who is offline gets it when they're back.
async fn ask_owner(conversation: [u8; 16], request: GroupRequest) -> Result<()> {
    let owner = Conversation::owner(&conversation)
        .ok_or_else(|| anyhow!("this group's owner has left, so its members can't change"))?;
    crate::messaging::send_control_wake_to(conversation, AppPayload::GroupRequest(request), owner).await
}

fn is_member(conversation: &[u8; 16], who: &[u8; 32]) -> bool {
    Conversation::members(conversation).iter().any(|m| m.active && m.member_ipk == *who)
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

/// The founder may not walk out of a group other people are still in.
///
/// Only their device commits membership changes, so their leaving would strand
/// everyone else in a group nobody can add to or remove from. Enforced here
/// rather than only in the UI: the FFI is reachable without it, and the group
/// this protects belongs to the other members as much as to the caller.
///
/// The way out is to empty the group first; handing it to another member is the
/// obvious next step and needs a wire event of its own.
///
/// Only binds someone still in the group. [`Conversation::is_owner`] reads the
/// active roster, so a founder who is already out owns nothing and their own
/// copy is theirs alone to drop.
pub(crate) fn require_not_stranding_the_group(
    conversation: &[u8; 16], who: &[u8; 32],
) -> Result<()> {
    let conn = crate::db::messages::MESSAGES_DB.lock();
    require_not_stranding_the_group_tx(&conn, conversation, who)
}

fn require_not_stranding_the_group_tx(
    conn: &rusqlite::Connection, conversation: &[u8; 16], who: &[u8; 32],
) -> Result<()> {
    if !Conversation::is_owner_tx(conn, conversation, who) {
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

fn require_owner(conversation: &[u8; 16], who: &[u8; 32]) -> Result<()> {
    if !Conversation::is_owner(conversation, who) {
        bail!("only the group's owner can make this change");
    }
    Ok(())
}

fn require_admin(conversation: &[u8; 16], who: &[u8; 32]) -> Result<()> {
    if !Conversation::is_admin(conversation, who) {
        bail!("only admins can remove members");
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

fn local_ipk() -> Result<[u8; 32]> {
    Ok(Identity::get().ok_or_else(|| anyhow!("identity not found"))?.ipk())
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
    crate::messaging::leaf_signer_for_group(provider, group, our_ipk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;

    /// Only the founder names admins, only an admin may have someone removed,
    /// and a member vouches only for Welcomes it asked for. Separate process:
    /// these read the real conversation tables and prefs.
    #[tokio::test(flavor = "current_thread")]
    async fn only_the_founder_names_admins() {
        const CHILD: &str = "PROMTUZ_GROUP_ADMINS_TEST";
        if std::env::var_os(CHILD).is_none() {
            let dir = std::env::temp_dir().join(format!("promtuz-group-admins-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&dir).unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "groups::tests::only_the_founder_names_admins", "--nocapture"])
                .env(CHILD, "1").env("PROMTUZ_DATA_DIR", &dir).output().unwrap();
            assert!(result.status.success(), "{}\n{}\nprofile: {}",
                String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr), dir.display());
            std::fs::remove_dir_all(dir).unwrap();
            return;
        }
        let (alice, bob, carol, dave, eve) = ([0xA1; 32], [0xB1; 32], [0xC1; 32], [0xD1; 32], [0xE1; 32]);
        Identity::save(crate::db::identity::IdentityRow {
            id: 0, ipk: bob, enc_isk: vec![], created_at: 0, name: "Bob".into(),
            avatar: None, avatar_revision: 0, bio: String::new(), profile_revision: 0,
        }).unwrap();
        let group = Conversation::join_group(&alice, &[alice, bob, carol, dave]).unwrap();
        Conversation::bind_group(&group, &[0x6A; 32]).unwrap();

        receive_admins(group, carol, vec![carol.into()]);
        assert!(!Conversation::is_admin(&group, &carol), "a member can't name admins");
        receive_admins(group, alice, vec![carol.into()]);
        assert_eq!(Conversation::admins(&group), vec![carol]);
        assert!(Conversation::is_owner(&group, &alice) && !Conversation::is_owner(&group, &carol));

        let add = |who: [u8; 32]| GroupRequest::Add { who: who.into() };
        let remove = |who: [u8; 32]| GroupRequest::Remove { who: who.into() };
        assert!(may_carry(&group, &dave, &add(eve)));
        assert!(!may_carry(&group, &eve, &add(dave)), "only members may ask");
        assert!(!may_carry(&group, &dave, &remove(bob)), "only admins may remove");
        assert!(may_carry(&group, &carol, &remove(dave)));
        assert!(!may_carry(&group, &carol, &remove(alice)), "nobody removes the founder");
        assert!(!may_carry(&group, &carol, &remove(carol)));
        assert!(may_carry(&group, &dave, &GroupRequest::Sync));
        assert!(!may_carry(&group, &eve, &GroupRequest::Sync));

        // We deliver only the founder's Welcomes, and only for adds we asked for.
        assert!(!may_forward(&group, &alice, &eve), "we never asked");
        crate::data::app_prefs::set(&asked_key(&group, &eve), "1").unwrap();
        assert!(!may_forward(&group, &carol, &eve), "only the founder's");
        assert!(may_forward(&group, &alice, &eve));

        // We are an ordinary member here.
        assert!(remove_member(group, dave).await.is_err());
        assert!(set_admin(group, dave, true).await.is_err());

        // A departed admin comes back as an ordinary member.
        Conversation::sync_roster(&group, &[alice, bob, dave]).unwrap();
        Conversation::sync_roster(&group, &[alice, bob, carol, dave]).unwrap();
        assert!(!Conversation::is_admin(&group, &carol));
        assert!(Conversation::is_admin(&group, &alice), "the founder stays an admin");
    }

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
