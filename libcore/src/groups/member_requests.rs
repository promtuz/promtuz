//! Identity-signed requests that survive loss or retirement of an MLS epoch, with no chat content.
//! A refresh needs a fresh member-owned KP; a leave still works after the leaver deleted its keys.

use anyhow::Result;
use anyhow::anyhow;
use common::proto::client_rel::Wake;
use common::proto::mls_wire::GroupChange;
use common::proto::mls_wire::GroupMemberAction;
use common::proto::mls_wire::GroupMemberRequest;
use common::proto::mls_wire::MlsEnvelopeP;
use common::proto::mls_wire::group_member_request_signing_input;
use common::proto::pack::Packer;
use common::utils::now_secs;
use ed25519_dalek::Signer;
use serde::Deserialize;
use serde::Serialize;

use crate::data::app_prefs;
use crate::data::conversation::Conversation;
use crate::db::outbox::OpType;
use crate::mls::MlsGroupHandle;
use crate::mls::PromtuzMlsProvider;
use crate::state::core;

#[derive(Serialize, Deserialize)]
struct Pending {
    request:    GroupMemberRequest,
    recipients: Vec<[u8; 32]>,
    last_sent:  u64,
}

fn key(gid: &[u8; 32], action: &GroupMemberAction) -> String {
    format!(
        "group_{}:{}",
        if *action == GroupMemberAction::Leave { "exit" } else { "refresh" },
        hex::encode(gid)
    )
}

fn load(gid: &[u8; 32], action: &GroupMemberAction) -> Option<Pending> {
    app_prefs::get(&key(gid, action))
        .and_then(|s| hex::decode(s).ok())
        .and_then(|b| postcard::from_bytes(&b).ok())
}

fn left_key(gid: &[u8; 32]) -> String {
    format!("group_left:{}", hex::encode(gid))
}

pub fn left(gid: &[u8; 32]) -> bool {
    app_prefs::get(&left_key(gid)).is_some() || load(gid, &GroupMemberAction::Leave).is_some()
}

/// Our leave reached the tree: keep that fact, drop the request behind it.
pub fn leave_carried(gid: &[u8; 32]) -> Result<()> {
    app_prefs::set(&left_key(gid), "1")?;
    app_prefs::remove(&key(gid, &GroupMemberAction::Leave))
}

/// A leave nobody carried in this long is not going to be; the request stops
/// waking former members while the marker keeps standing.
const LEAVE_REQUEST_LIFE_MS: u64 = 30 * 24 * 3_600_000;

/// The minting time a UUIDv7 nonce carries.
fn minted_ms(nonce: &[u8; 16]) -> u64 {
    u64::from_be_bytes([0, 0, nonce[0], nonce[1], nonce[2], nonce[3], nonce[4], nonce[5]])
}

pub fn refreshed(gid: &[u8; 32]) -> Result<()> {
    app_prefs::remove(&key(gid, &GroupMemberAction::Refresh))
}

pub fn refresh_request(gid: &[u8; 32]) -> Option<GroupMemberRequest> {
    load(gid, &GroupMemberAction::Refresh).map(|p| p.request)
}

pub fn known_anchor(gid: &[u8; 32]) -> Option<[u8; 32]> {
    app_prefs::get(&format!("group_anchor:{}", hex::encode(gid)))
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| b.try_into().ok())
}

pub fn anchor(gid: &[u8; 32], root: [u8; 32]) -> Result<()> {
    let key = format!("group_anchor:{}", hex::encode(gid));
    if let Some(old) = app_prefs::get(&key) {
        anyhow::ensure!(old == hex::encode(root), "invitation changes the group's identity");
        return Ok(());
    }
    app_prefs::set(&key, &hex::encode(root))
}

fn begin(conversation: [u8; 16], action: GroupMemberAction) -> Result<()> {
    let gid = super::require_group(&conversation)?;
    if left(&gid) && action == GroupMemberAction::Refresh {
        return Ok(());
    }
    if load(&gid, &action).is_none() {
        let (me, signer) = super::local_signer()?;
        let nonce = crate::data::message::next_dispatch_id();
        let signature =
            signer.sign(&group_member_request_signing_input(&gid, &me, &nonce, &action));
        let pending = Pending {
            request:    GroupMemberRequest {
                who:       me.into(),
                nonce:     nonce.into(),
                action:    action.clone(),
                signature: signature.to_bytes().into(),
            },
            recipients: Conversation::recipients(&conversation),
            last_sent:  0,
        };
        app_prefs::set(&key(&gid, &action), &hex::encode(postcard::to_allocvec(&pending)?))?;
    }
    let action = action.clone();
    core().spawn(async move {
        if let Err(e) = send(gid, action).await {
            log::debug!("GROUP: member request remains pending: {e}");
        }
    });
    Ok(())
}

pub fn leave(conversation: [u8; 16]) -> Result<()> {
    begin(conversation, GroupMemberAction::Leave)
}

pub fn refresh(conversation: [u8; 16]) -> Result<()> {
    begin(conversation, GroupMemberAction::Refresh)
}

async fn send(gid: [u8; 32], action: GroupMemberAction) -> Result<()> {
    let Some(mut pending) = load(&gid, &action) else { return Ok(()) };
    let now = now_secs();
    if now.saturating_sub(pending.last_sent) < 60 {
        return Ok(());
    }
    if action == GroupMemberAction::Leave
        && (now * 1000).saturating_sub(minted_ms(&pending.request.nonce.0)) > LEAVE_REQUEST_LIFE_MS
    {
        return Ok(());
    }
    let (me, signer) = super::local_signer()?;
    let bytes =
        MlsEnvelopeP::GroupMemberRequest { group: gid.into(), request: pending.request.clone() }
            .ser()?;
    let id = crate::data::message::next_dispatch_id();
    let mut copies = pending
        .recipients
        .iter()
        .map(|to| {
            Ok((
                *to,
                id,
                OpType::Control,
                crate::delivery::prepare_dispatch(
                    to,
                    &me,
                    &signer,
                    &id,
                    bytes.clone(),
                    Wake::Message,
                    0,
                )?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    crate::delivery::enqueue_batch(&mut copies)?;
    pending.last_sent = now;
    app_prefs::set(&key(&gid, &action), &hex::encode(postcard::to_allocvec(&pending)?))?;
    super::recovery::dispatch(copies).await;
    Ok(())
}

pub fn resume() {
    for (prefix, action) in
        [("group_exit:", GroupMemberAction::Leave), ("group_refresh:", GroupMemberAction::Refresh)]
    {
        for (name, _) in app_prefs::with_prefix(prefix) {
            let Some(gid) = name
                .strip_prefix(prefix)
                .and_then(|s| hex::decode(s).ok())
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
            else {
                continue;
            };
            let action = action.clone();
            core().spawn(async move {
                let _ = send(gid, action).await;
            });
        }
    }
}

pub fn received(gid: [u8; 32], request: GroupMemberRequest) -> Result<()> {
    crate::mls::branch_proof::verify_member_request(&gid, &request)?;
    let provider = PromtuzMlsProvider::shared();
    let Some(group) = MlsGroupHandle::load(&provider, &gid)? else { return Ok(()) };
    if !group.roster().contains(&request.who.0) {
        return Ok(());
    }
    let conversation =
        Conversation::for_group(&gid).ok_or_else(|| anyhow!("group has no conversation"))?;
    let me = super::local_ipk()?;
    if request.who.0 == me {
        return Ok(());
    }
    let state = group.group_meta().ok_or_else(|| anyhow!("not a group chat"))?.effective();
    let may_carry = state.committer == me
        || (state.committer == request.who.0
            && (state.role(&me) >= super::ROLE_ADMIN
                || state.owners.len() + state.admins.len() == 1));
    if may_carry {
        if completed(&provider, &gid, &request)? {
            return Ok(());
        }
        super::remember(&conversation, GroupChange::MemberRequest(request))?;
        super::resume(conversation);
    } else {
        // The requester may predate the current committer. A known member can
        // relay the original signature; it cannot alter the requested action.
        core().spawn(async move {
            let Ok((me, signer)) = super::local_signer() else { return };
            let Ok(payload) =
                (MlsEnvelopeP::GroupMemberRequest { group: gid.into(), request }).ser()
            else {
                return;
            };
            let id = crate::data::message::next_dispatch_id();
            crate::delivery::dispatch_to_member(
                &state.committer,
                &me,
                &signer,
                &id,
                payload,
                OpType::Control,
                Wake::Message,
                0,
            )
            .await;
        });
    }
    Ok(())
}

pub fn completed(
    provider: &PromtuzMlsProvider, gid: &[u8; 32], request: &GroupMemberRequest,
) -> Result<bool> {
    let Some(group) = MlsGroupHandle::load(provider, gid)? else { return Ok(false) };
    let history = crate::mls::recovery::history(provider, gid, group.branch_id())?;
    Ok(crate::mls::branch_proof::member_requests(&history)?
        .iter()
        .any(|old| old.who == request.who && old.nonce == request.nonce))
}
