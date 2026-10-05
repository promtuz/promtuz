//! Messaging exports: sends and typed read paths.

use common::types::bytes::fixed;

use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::message::Message;
use crate::db::messages::MessageRow;
use crate::platform::CoreError;
use crate::state::core;

#[derive(uniffi::Record)]
pub struct MessageRecord {
    pub id: String,
    /// 16 bytes, stable for the conversation's life.
    pub conversation_id: Vec<u8>,
    /// `None` means us, as on every outgoing row.
    pub sender_ipk: Option<Vec<u8>>,
    pub content: String,
    pub outgoing: bool,
    pub timestamp: u64,
    /// 0 = pending, 1 = sent, 2 = failed, 3 = delivered, 4 = read.
    pub status: u8,
    /// 16-byte shared id that edits and deletes target; `None` on legacy rows.
    pub dispatch_id: Option<Vec<u8>>,
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
    /// dispatch_id of the quoted message, when this is a reply.
    pub reply_to: Option<Vec<u8>>,
    /// 0 for an ordinary message; otherwise a membership or title change where `sender_ipk` acted
    /// and `content` is the target: a hex IPK, or the new title for a rename.
    pub system: u8,
    /// When this row heads an album, the dispatch ids it collapses, itself first; empty otherwise.
    pub album_items: Vec<Vec<u8>>,
    /// Folded into the album above it; clients skip it.
    pub in_album: bool,
    /// Media kind (1 image, 2 attachment, 3 voice), 0 for none. Filled only where the row stands
    /// alone as one line: the home list and notifications.
    pub media_kind: u8,
}

/// `mine` is `reactor == self`, precomputed so the UI need not hold its own IPK.
#[derive(uniffi::Record)]
pub struct ReactionRecord {
    pub dispatch_id: Vec<u8>,
    pub reactor: Vec<u8>,
    pub emoji: String,
    pub timestamp: u64,
    pub mine: bool,
}

#[derive(uniffi::Record)]
pub struct UnreadCount {
    pub conversation_id: Vec<u8>,
    pub count: u32,
}

#[derive(uniffi::Record)]
pub struct ConversationRecord {
    pub id: Vec<u8>,
    /// 0 = direct (a 1:1 chat), 1 = group.
    pub kind: u8,
    /// The group name as set: empty until named, and for a direct chat. What a rename field edits.
    pub title: String,
    /// What to call the chat on screen; an unnamed group falls back to its members' names.
    pub display_name: String,
    /// Active roster, us included. Two entries for a direct chat.
    pub members: Vec<Vec<u8>>,
    /// The other party of a direct chat; `None` for a group.
    pub peer: Option<Vec<u8>>,
    /// The active roster minus us: who a send fans out to, and whose presence and typing to show.
    pub others:         Vec<Vec<u8>>,
    /// We are an admin or owner: we may remove members and change the group's rules.
    pub can_manage:     bool,
    /// Our role in a group: 0 member, 1 admin, 2 owner.
    pub role:           u8,
    /// `None` for a direct chat, and for a group from before signed rules, which its founder runs.
    pub rules:          Option<GroupRulesRecord>,
    pub can_add:        bool,
    /// We may rename the group and change its photo.
    pub can_edit:       bool,
    /// False in a group where only admins send, and once we left.
    pub can_send:       bool,
    /// Whose phone makes the group's changes. What anyone else asks for waits
    /// until it's online.
    pub committer:      Option<Vec<u8>>,
    /// Ours does, and others are in the group: deleting the chat has to leave
    /// it first, which hands that on.
    pub commits:        bool,
    /// An MLS group backs this conversation, so it can send.
    pub has_group: bool,
    /// False for a group we left or were removed from, which keeps its history.
    pub am_member: bool,
    /// False for a direct chat and for a group we already left; see [`Self::owner_is_stuck`].
    pub can_leave:      bool,
    /// We founded a group from before signed rules that others are still in, so leaving and
    /// deleting are refused until everyone else is removed or the group converts.
    pub owner_is_stuck: bool,
    /// Core already sorts pinned chats first.
    pub pinned: bool,
    pub muted: bool,
    /// Newest message already alerted for, unix seconds.
    pub alerted_at: u64,
    pub created_at: u64,
    /// A message request we have not accepted: its own list, and no composer.
    pub request:        bool,
}

/// What members who aren't admins may do in a group.
#[derive(uniffi::Record)]
pub struct GroupRulesRecord {
    pub members_add:    bool,
    pub members_edit:   bool,
    pub members_send:   bool,
    /// Admins may make others admins. Only owners change this.
    pub admins_appoint: bool,
}

#[derive(uniffi::Record)]
pub struct MemberRecord {
    pub ipk:             Vec<u8>,
    /// 0 = member, 1 = admin, 2 = owner.
    pub role:            u8,
    pub joined_at:       u64,
    /// False once they left or were removed; their old messages still attribute.
    pub active: bool,
    pub me: bool,
    pub name: String,
    /// The name came from them, not the address book, so we cannot vouch for it.
    pub name_is_claimed: bool,
}

#[derive(uniffi::Record)]
pub struct ContactInfo {
    pub ipk: Vec<u8>,
    pub name: String,
    pub added_at: u64,
    /// 0 = pending, 1 = paired, 2 = rejected.
    pub status: u8,
    /// Why rejected (a DECLINE_* code), when status = 2.
    pub reject_reason: Option<u8>,
}

/// The row is saved before this returns, so an error means nothing was kept. Delivery is
/// reported through `CoreEvents::on_message`.
#[uniffi::export]
pub fn send_message(
    conversation_id: Vec<u8>, content: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let reply = reply_to.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    let msg = Message::save_outgoing(conv, &content, reply)?;
    core().spawn(async move {
        let sent = async {
            let payload = crate::messaging::body::rebuild_pending_payload(&conv, &msg)?;
            crate::messaging::send_prepared(conv, &msg, payload).await
        };
        if let Err(e) = sent.await {
            log::error!("MESSAGE: send failed: {e}");
        }
    });
    Ok(())
}

/// Edit text or a media caption, preserving its body. Local validation and
/// persistence finish before returning; propagation to peers is asynchronous.
#[uniffi::export]
pub fn edit_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, content: String,
) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let target = fixed::<16>(&dispatch_id, "dispatch_id")?;
    let body = crate::messaging::text_edit_body(&conv, &target, content)?;
    let applied = crate::messaging::body::apply_revise_body(&conv, &target, body.clone(), true, None)?;
    let (row, content) = applied.ok_or_else(|| anyhow::anyhow!("message cannot be edited"))?;
    use crate::events::Emittable;
    crate::events::messaging::MessageEv::Edited { id: row.id, conversation: conv, content }.emit();
    core().spawn(async move {
        if let Err(e) = crate::messaging::send_control(
            conv,
            common::proto::mls_wire::AppPayload::Revise { target, body },
        )
        .await
        {
            log::error!("MESSAGE: edit failed: {e}");
        }
    });
    Ok(())
}

/// `activity` is an OR of `ACTIVITY_*` bits, `0` meaning idle. Dropped if either side is offline.
#[uniffi::export]
pub fn set_activity(conversation_id: Vec<u8>, activity: u16) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    core().spawn(async move {
        let session = core().session();
        if let Err(e) = crate::presence::set_activity(session.as_deref(), conv, activity).await {
            log::debug!("MESSAGE: set_activity failed: {e}");
        }
    });
    Ok(())
}

/// One person may stack several distinct emoji on a message. Surfaces via `on_reaction`.
#[uniffi::export]
pub fn react_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, emoji: String, add: bool,
) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let target = fixed::<16>(&dispatch_id, "dispatch_id")?;
    core().spawn(async move {
        if let Err(e) = crate::messaging::react(conv, target, emoji, add).await {
            log::error!("MESSAGE: react failed: {e}");
        }
    });
    Ok(())
}

/// All reactions in a conversation, oldest first.
#[uniffi::export]
pub fn reactions_for(conversation_id: Vec<u8>) -> Result<Vec<ReactionRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let me = crate::data::identity::Identity::local_ipk();
    Ok(crate::data::reaction::Reaction::for_conversation(&conv)
        .into_iter()
        .map(|r| ReactionRecord {
            mine: me.as_ref().is_some_and(|m| m == &r.reactor),
            dispatch_id: r.dispatch_id,
            reactor: r.reactor.to_vec(),
            emoji: r.emoji,
            timestamp: r.timestamp,
        })
        .collect())
}

/// Mark messages through the selected local arrival read, recording event
/// times before asynchronous encrypted receipt dispatch.
#[uniffi::export]
pub fn mark_read(conversation_id: Vec<u8>, upto_dispatch_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    crate::data::receipts::read(&conv, &fixed::<16>(&upto_dispatch_id, "dispatch_id")?)?;
    crate::data::receipts::schedule();
    Ok(())
}

#[uniffi::export]
pub fn mark_conversation_read(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    if let Some(upto) = Message::newest_incoming_dispatch(&conv) {
        crate::data::receipts::read(&conv, &upto)?;
        crate::data::receipts::schedule();
    }
    Ok(())
}

#[uniffi::export]
pub fn message_receipt_info(
    conversation_id: Vec<u8>, dispatch_ids: Vec<Vec<u8>>,
) -> Result<crate::data::receipts::MessageReceiptInfo, CoreError> {
    let ids = dispatch_ids
        .iter()
        .map(|id| fixed::<16>(id, "dispatch_id"))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(crate::data::receipts::info_many(&conv, &ids)?)
}

/// Only conversations with unread incoming messages.
#[uniffi::export]
pub fn unread_counts() -> Vec<UnreadCount> {
    Message::unread_counts()
        .into_iter()
        .map(|(conv, count)| UnreadCount { conversation_id: conv.to_vec(), count })
        .collect()
}

/// Replaces the prior interest set. Each peer independently authorizes its own publication.
#[uniffi::export]
pub fn subscribe_presence(contacts: Vec<Vec<u8>>) -> Result<(), CoreError> {
    let list = contacts.iter().map(|c| fixed::<32>(c, "ipk")).collect::<Result<Vec<_>, _>>()?;
    core().spawn(async move {
        let session = core().session();
        if let Err(e) = crate::presence::subscribe_presence(session.as_deref(), list).await {
            log::debug!("PRESENCE: subscribe failed: {e}");
        }
    });
    Ok(())
}

/// `idle` is true on backgrounding and false on foregrounding.
#[uniffi::export]
pub fn set_presence(idle: bool) {
    // Apply the local upload policy synchronously, before any relay work.
    crate::transfer::sharing::set_foreground(core(), !idle);
    core().spawn(async move {
        let session = core().session();
        if let Err(e) = crate::presence::set_presence(session.as_deref(), idle).await {
            log::debug!("PRESENCE: set_presence failed: {e}");
        }
    });
}

/// Fetch queued messages even when a push wakes an already-connected process.
#[uniffi::export(async_runtime = "tokio")]
pub async fn sync_messages() -> Result<(), CoreError> {
    crate::api::init::on_foreground();
    let ipk = crate::data::identity::Identity::public_key().map_err(anyhow::Error::from)?;
    // Replacing an Android wake job must not cancel a message mid-decryption, so core runs this
    // bounded sync on its own task.
    on_runtime(async move {
        tokio::time::timeout(std::time::Duration::from_secs(45), async {
            loop {
                let session = core().session();
                if let Some(session) = session.filter(|s| s.conn.close_reason().is_none()) {
                    return session.sync_incoming(ipk).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(anyhow::Error::from)?
    })
    .await
}

#[uniffi::export]
pub fn pending_notification_ids(conversation_id: Vec<u8>) -> Result<Vec<String>, CoreError> {
    Ok(Message::pending_notification_ids(&fixed::<16>(&conversation_id, "conversation id")?)?)
}

#[uniffi::export]
pub fn mark_notified(ids: Vec<String>) -> Result<(), CoreError> {
    Ok(Message::mark_notified(&ids)?)
}

/// Registers our push pseudonym with the home relay; this also runs on every connect.
#[uniffi::export]
pub fn register_push() {
    core().spawn(async {
        if let Err(e) = crate::push::register_push().await {
            log::debug!("PUSH: register failed: {e}");
        }
    });
}

/// Returns after a gateway acknowledges the token, so the platform can retry a failed setup later.
#[uniffi::export(async_runtime = "tokio")]
pub async fn register_push_token(token: Vec<u8>) -> Result<(), CoreError> {
    Ok(crate::push::set_push_token(token).await?)
}

/// `for_everyone` tombstones it everywhere; otherwise it is removed locally.
#[uniffi::export]
pub fn delete_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, for_everyone: bool,
) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let target = fixed::<16>(&dispatch_id, "dispatch_id")?;
    core().spawn(async move {
        if let Err(e) = crate::messaging::delete(conv, target, for_everyone).await {
            log::error!("MESSAGE: delete failed: {e}");
        }
    });
    Ok(())
}

#[derive(uniffi::Record)]
pub struct SearchHit {
    pub dispatch_id: Vec<u8>,
    /// Messages in the chat newer than this one.
    pub newer: u32,
}

/// Messages in a conversation containing `query`, newest first.
#[uniffi::export]
pub fn search_messages(
    conversation_id: Vec<u8>, query: String, limit: u32,
) -> Result<Vec<SearchHit>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(Message::search(&conv, &query, limit)
        .into_iter()
        .map(|(did, newer)| SearchHit { dispatch_id: did.to_vec(), newer })
        .collect())
}

/// A calendar target, including its pagination depth. IDs keep the target
/// stable if newer messages arrive while the client widens its window.
#[derive(uniffi::Record)]
pub struct MessagePosition {
    pub id: String,
    pub dispatch_id: Option<Vec<u8>>,
    pub newer: u32,
}

#[uniffi::export]
pub fn message_at_time(
    conversation_id: Vec<u8>, timestamp: u64,
) -> Result<Option<MessagePosition>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(Message::position_at_time(&conv, timestamp)?.map(|(id, dispatch_id, newer)| MessagePosition { id, dispatch_id, newer }))
}

/// Paginated history, oldest-first. `before_id` pages backwards by ULID;
/// pass an empty string for the latest page.
#[uniffi::export]
pub fn get_messages(
    conversation_id: Vec<u8>, limit: u32, before_id: String,
) -> Result<Vec<MessageRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let mut rows: Vec<MessageRecord> =
        Message::get_messages(&conv, limit, &before_id).into_iter().map(Into::into).collect();
    mark_albums(&conv, &mut rows);
    Ok(rows)
}

/// Folds consecutive rows sharing a media `group_id` into one album; a reply between them splits it
/// into two.
fn mark_albums(conversation: &[u8; 16], rows: &mut [MessageRecord]) {
    let groups = crate::data::media::groups_for(conversation).unwrap_or_default();
    if groups.is_empty() {
        return;
    }
    let group_of = |r: &MessageRecord| r.dispatch_id.as_ref().and_then(|d| groups.get(d)).cloned();

    let mut i = 0;
    while i < rows.len() {
        let Some(g) = group_of(&rows[i]) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < rows.len() && group_of(&rows[j]).as_ref() == Some(&g) {
            j += 1;
        }
        if j - i > 1 {
            rows[i].album_items =
                rows[i..j].iter().filter_map(|r| r.dispatch_id.clone()).collect();
            let statuses: Vec<u8> = rows[i..j].iter().map(|r| r.status).collect();
            rows[i].status = crate::data::receipts::combined_status(&statuses, true);
            rows[i + 1..j].iter_mut().for_each(|r| r.in_album = true);
        }
        i = j;
    }
}

/// Every conversation, most recently active first.
#[uniffi::export]
pub fn list_conversations() -> Vec<ConversationRecord> {
    Conversation::list()
        .into_iter()
        .filter(|c| !crate::groups::delete_pending(&c.id))
        .map(conversation_record)
        .collect()
}

fn conversation_record(c: crate::db::messages::ConversationRow) -> ConversationRecord {
    let others = Conversation::recipients(&c.id);
    let me = crate::data::identity::Identity::local_ipk();
    let roster = Conversation::members(&c.id);
    let is_group = c.kind == crate::data::conversation::KIND_GROUP;
    let am_member = me.is_some_and(|k| roster.iter().any(|m| m.active && m.member_ipk == k))
        && !crate::groups::is_leaving(&c.id);
    let state = Conversation::state(&c.id).filter(|_| am_member);
    let signed = Conversation::has_signed_rules(&c.id);
    let role = me.zip(state.as_ref()).map_or(0, |(k, s)| s.role(&k));
    let owner_is_stuck =
        !signed && role == crate::data::conversation::ROLE_OWNER && !others.is_empty();

    ConversationRecord {
        members: roster.iter().filter(|m| m.active).map(|m| m.member_ipk.to_vec()).collect(),
        peer: Conversation::peer_of(&c.id).map(|p| p.to_vec()),
        can_manage: role >= crate::data::conversation::ROLE_ADMIN,
        role,
        rules: state.as_ref().filter(|_| signed).map(|s| GroupRulesRecord {
            members_add:    s.rules.members_add,
            members_edit:   s.rules.members_edit,
            members_send:   s.rules.members_send,
            admins_appoint: s.rules.admins_appoint,
        }),
        can_add: me.zip(state.as_ref()).is_some_and(|(k, s)| s.may_add(&k))
            && c.mls_group_id.is_some(),
        can_edit: me.zip(state.as_ref()).is_some_and(|(k, s)| s.may_edit(&k)),
        can_send: am_member && me.zip(state.as_ref()).is_none_or(|(k, s)| s.may_send(&k)),
        committer: state.as_ref().map(|s| s.committer.to_vec()),
        commits: signed
            && !others.is_empty()
            && me.zip(state.as_ref()).is_some_and(|(k, s)| s.committer == k),
        has_group: c.mls_group_id.is_some(),
        am_member,
        can_leave:      is_group && am_member && !owner_is_stuck,
        owner_is_stuck,
        display_name:   display_name(&c, &others),
        pinned:         c.pinned,
        muted:          c.muted,
        alerted_at:     c.alerted_at,
        others:         others.into_iter().map(|p| p.to_vec()).collect(),
        id:             c.id.to_vec(),
        kind:           c.kind,
        request:        !is_group && crate::requests::is_request_chat(&c.id),
        title:          c.title,
        created_at:     c.created_at,
    }
}

/// A group's name can be missing when the rename never reached us, so fall back to its members.
fn display_name(c: &crate::db::messages::ConversationRow, others: &[[u8; 32]]) -> String {
    if !c.title.is_empty() {
        return c.title.clone();
    }
    let mut names: Vec<String> =
        others.iter().map(crate::data::peer_name::resolve).collect();
    names.sort();
    match names.len() {
        0 => String::new(),
        1..=3 => names.join(", "),
        // Past three the list stops being a name and starts being a roster.
        _ => format!("{}, {} and {} more", names[0], names[1], names.len() - 2),
    }
}

/// Local and silent: the group's keys go too, so nothing from it reaches this device again. `force`
/// skips the founder's stranding guard, for a group too broken to manage or leave.
#[uniffi::export]
pub fn delete_conversation(conversation_id: Vec<u8>, force: bool) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let Some(row) = Conversation::get(&conv) else { return Ok(()) };
    if crate::groups::is_leaving(&conv) {
        crate::groups::delete_after_leave(&conv)?;
        return Ok(());
    }
    let me = crate::data::identity::Identity::local_ipk();

    if let Some(me) = me
        && !force
    {
        crate::groups::require_not_stranding_the_group(&conv, &me)?;
    }
    if let Some(gid) = Conversation::group_of(&conv) {
        purge_mls_group(&gid);
    }
    crate::groups::forget_requests(&conv);
    Conversation::delete(&conv)?;
    log::info!(
        "DELETE: dropped conversation {} ({})",
        hex::encode(&conv[..4]),
        if row.kind == crate::data::conversation::KIND_GROUP { "group" } else { "direct" }
    );
    Ok(())
}

pub(crate) fn purge_mls_group(gid: &[u8; 32]) {
    if let Err(e) = crate::mls::PromtuzMlsProvider::shared().storage().forget_group(gid) {
        log::warn!("MLS: clearing group storage rows failed: {e}");
    }
}

/// Local only, and allowed even for a founder since it changes no membership.
#[uniffi::export]
pub fn clear_conversation_history(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    Ok(Conversation::clear_history(&fixed::<16>(&conversation_id, "conversation id")?)?)
}

#[uniffi::export]
pub fn set_conversation_pinned(conversation_id: Vec<u8>, pinned: bool) -> Result<(), CoreError> {
    Ok(Conversation::set_pinned(&fixed::<16>(&conversation_id, "conversation id")?, pinned)?)
}

#[uniffi::export]
pub fn set_conversation_muted(conversation_id: Vec<u8>, muted: bool) -> Result<(), CoreError> {
    Ok(Conversation::set_muted(&fixed::<16>(&conversation_id, "conversation id")?, muted)?)
}

#[uniffi::export]
pub fn set_alerted_at(conversation_id: Vec<u8>, ts_secs: u64) -> Result<(), CoreError> {
    Ok(Conversation::set_alerted_at(&fixed::<16>(&conversation_id, "conversation id")?, ts_secs)?)
}

#[uniffi::export]
pub fn get_pref(key: String) -> Option<String> {
    crate::data::app_prefs::get(&key)
}

/// Kept in core rather than platform preferences so the backup carries it across a reinstall.
#[uniffi::export]
pub fn set_pref(key: String, value: String) -> Result<(), CoreError> {
    Ok(crate::data::app_prefs::set(&key, &value)?)
}

/// The last `limit` incoming, undeleted messages, oldest first: what a notification summarises.
#[uniffi::export]
pub fn recent_incoming(
    conversation_id: Vec<u8>, limit: u32,
) -> Result<Vec<MessageRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(Message::recent_incoming(&conv, limit).into_iter().map(with_media_kind).collect())
}

/// The direct conversation with `peer_ipk`, created on first open.
#[uniffi::export]
pub fn conversation_with(peer_ipk: Vec<u8>) -> Result<Vec<u8>, CoreError> {
    let peer = fixed::<32>(&peer_ipk, "ipk")?;
    Ok(Conversation::for_peer(&peer)?.to_vec())
}

#[uniffi::export]
pub fn get_conversation(conversation_id: Vec<u8>) -> Result<Option<ConversationRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(Conversation::get(&conv).map(conversation_record))
}

/// Full roster including departed members, so historic messages still
/// attribute to a name.
#[uniffi::export]
pub fn conversation_members(conversation_id: Vec<u8>) -> Result<Vec<MemberRecord>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let me = crate::data::identity::Identity::local_ipk();
    Ok(Conversation::members(&conv)
        .into_iter()
        .map(|m| {
            let (name, name_is_claimed) = crate::data::peer_name::resolve_claimed(&m.member_ipk);
            MemberRecord {
                me: me.is_some_and(|k| k == m.member_ipk),
                name,
                name_is_claimed,
                ipk: m.member_ipk.to_vec(),
                role: m.role,
                joined_at: m.joined_at,
                active: m.active,
            }
        })
        .collect())
}

/// Applied locally at once, then announced to a group's members. A direct chat's title stays local.
#[uniffi::export]
pub fn set_conversation_title(conversation_id: Vec<u8>, title: String) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let is_group =
        Conversation::get(&conv).is_some_and(|c| c.kind == crate::data::conversation::KIND_GROUP);
    let me = crate::data::identity::Identity::local_ipk().unwrap_or_default();
    if is_group && !Conversation::may_edit(&conv, &me) {
        return Err(anyhow::anyhow!("only admins can rename this group").into());
    }
    Conversation::set_title(&conv, &title)?;
    if is_group {
        core().spawn(async move {
            crate::groups::announce(
                conv,
                common::proto::mls_wire::SystemEvent::Titled { title },
            )
            .await;
        });
    }
    Ok(())
}

/// The latest message of each conversation.
#[uniffi::export]
pub fn get_conversations() -> Vec<MessageRecord> {
    Message::get_conversations().into_iter().map(with_media_kind).collect()
}

fn with_media_kind(row: MessageRow) -> MessageRecord {
    let mut rec = MessageRecord::from(row);
    if let Some(did) = rec.dispatch_id.as_deref().and_then(|d| <[u8; 16]>::try_from(d).ok())
        && let Ok(conv) = fixed::<16>(&rec.conversation_id, "conversation id")
    {
        rec.media_kind = crate::data::media::kind(&conv, &did).unwrap_or(0);
    }
    rec
}

/// All contacts, newest first. Unaccepted requests are not contacts.
#[uniffi::export]
pub fn get_contacts() -> Vec<ContactInfo> {
    Contact::list()
        .into_iter()
        .filter(|c| c.status != crate::data::contact::PAIR_STATUS_REQUEST)
        .map(|c| ContactInfo {
            ipk: c.ipk.to_vec(),
            name: crate::data::peer_name::resolve(&c.ipk),
            added_at: c.added_at,
            status: c.status,
            reject_reason: c.reject_reason,
        })
        .collect()
}

/// A contact enriched with per-store diagnostics for a debug UI.
#[derive(uniffi::Record)]
pub struct ContactDiag {
    pub ipk: Vec<u8>,
    pub name: String,
    /// True once an MLS group id is bound.
    pub paired: bool,
    /// Current MLS epoch, `None` if unpaired or the group can't load.
    pub epoch: Option<u64>,
    pub message_count: u32,
    /// Newest message status, coded as in [`MessageRecord::status`].
    pub last_status: Option<u8>,
    /// Pending (undelivered) outbox ops for this peer.
    pub pending_ops: u32,
}

/// Deletes all per-contact state so re-scanning their QR is a clean first-time add. Best-effort and
/// idempotent: a failing store is logged and the rest still goes.
#[uniffi::export(async_runtime = "tokio")]
pub async fn forget_contact(ipk: Vec<u8>) -> Result<(), CoreError> {
    let ipk = fixed::<32>(&ipk, "ipk")?;
    let Some(contact) = Contact::get(&ipk) else { return Ok(()) };

    // Queued copies are moot; dropping them before the `Unpaired` notice keeps the notice. It
    // tells a paired contact to start fresh, so their next message arrives as a request.
    crate::delivery::forget_target(&ipk);
    if contact.inner.status == crate::data::contact::PAIR_STATUS_PAIRED && contact.inner.mls_group_id.is_some()
        && let Ok(conv) = Conversation::for_peer(&ipk)
        && let Err(e) = crate::messaging::send_control(conv, common::proto::mls_wire::AppPayload::Unpaired).await
    {
        log::debug!("FORGET: could not tell them the pair ended: {e}");
    }

    // Read before the contact row goes, and cleared as deeply as a conversation delete.
    if let Some(gid) = contact.inner.mls_group_id {
        purge_mls_group(&gid);
    }

    if let Ok(conv) = Conversation::for_peer(&ipk) {
        if let Err(e) = Conversation::delete(&conv) {
            log::error!("FORGET: conversation delete failed: {e}");
        }
    }
    // Sever any live direct link so a forgotten contact can't keep talking
    // over an already-open P2P connection.
    crate::p2p::drop_link(&ipk);
    if let Err(e) = Contact::delete(&ipk) {
        log::error!("FORGET: contact delete failed: {e}");
    }
    Ok(())
}

#[uniffi::export]
pub fn list_contacts_diag() -> Vec<ContactDiag> {
    let provider = crate::mls::PromtuzMlsProvider::shared();
    Contact::list()
        .into_iter()
        .map(|c| {
            let epoch = c.mls_group_id.and_then(|gid| {
                crate::mls::MlsGroupHandle::load(&provider, &gid).ok().flatten().map(|g| g.epoch())
            });
            ContactDiag {
                paired: c.mls_group_id.is_some(),
                epoch,
                message_count: Conversation::for_peer(&c.ipk)
                    .map(|conv| Message::count_in(&conv))
                    .unwrap_or(0),
                last_status: Conversation::for_peer(&c.ipk)
                    .ok()
                    .and_then(|conv| Message::last_status_in(&conv)),
                pending_ops: crate::delivery::pending_ops_for(&c.ipk),
                ipk: c.ipk.to_vec(),
                name: crate::data::peer_name::resolve(&c.ipk),
            }
        })
        .collect()
}

impl From<MessageRow> for MessageRecord {
    fn from(r: MessageRow) -> Self {
        MessageRecord {
            id: r.id.to_string(),
            conversation_id: r.conversation_id.to_vec(),
            sender_ipk: r.sender_ipk,
            content: r.content,
            outgoing: r.outgoing,
            timestamp: r.timestamp,
            status: r.status,
            dispatch_id: r.dispatch_id,
            edited: r.edited,
            deleted: r.deleted,
            reply_to: r.reply_to,
            system: r.system,
            album_items: Vec::new(),
            in_album: false,
            media_kind: 0,
        }
    }
}

/// uniffi polls plain async exports on its own executor, where QUIC I/O has no Tokio reactor.
pub(crate) async fn on_runtime<T, F>(fut: F) -> Result<T, CoreError>
where
    T: Send + 'static,
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
{
    match core().spawn(fut).await {
        Ok(Some(result)) => result.map_err(CoreError::from),
        Ok(None) => Err(CoreError::Internal { msg: "core stopped".into() }),
        Err(e) => Err(CoreError::Internal { msg: format!("core task did not finish: {e}") }),
    }
}

/// Returns the new conversation id, ready to send in.
#[uniffi::export]
pub async fn create_group(title: String, members: Vec<Vec<u8>>) -> Result<Vec<u8>, CoreError> {
    let list = members.iter().map(|m| fixed::<32>(m, "ipk")).collect::<Result<Vec<_>, _>>()?;
    let id = on_runtime(crate::groups::create_group(title, list)).await?;
    Ok(id.to_vec())
}

// The changes below return whether they are done. Only the committer's phone changes a group;
// `false` means we asked it to.

/// One change; the new members get no pre-join history.
#[uniffi::export]
pub async fn add_group_members(
    conversation_id: Vec<u8>, members: Vec<Vec<u8>>,
) -> Result<bool, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let who = members.iter().map(|m| fixed::<32>(m, "ipk")).collect::<Result<Vec<_>, _>>()?;
    on_runtime(crate::groups::add_members(conv, who)).await
}

/// The removing commit refreshes the group's keys, so their device cannot read what follows.
#[uniffi::export]
pub async fn remove_group_member(
    conversation_id: Vec<u8>, member_ipk: Vec<u8>,
) -> Result<bool, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let who = fixed::<32>(&member_ipk, "ipk")?;
    on_runtime(crate::groups::remove_member(conv, who)).await
}

/// `role` is 0 member, 1 admin or 2 owner.
#[uniffi::export]
pub async fn set_group_role(
    conversation_id: Vec<u8>, member_ipk: Vec<u8>, role: u8,
) -> Result<bool, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let who = fixed::<32>(&member_ipk, "ipk")?;
    on_runtime(crate::groups::set_role(conv, who, role)).await
}

#[uniffi::export]
pub async fn set_group_rules(
    conversation_id: Vec<u8>, rules: GroupRulesRecord,
) -> Result<bool, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let rules = common::proto::mls_wire::GroupRules {
        members_add:    rules.members_add,
        members_edit:   rules.members_edit,
        members_send:   rules.members_send,
        admins_appoint: rules.admins_appoint,
    };
    on_runtime(crate::groups::set_rules(conv, rules)).await
}

/// The conversation and its history stay; it just can no longer send.
#[uniffi::export]
pub async fn leave_group(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    on_runtime(crate::groups::leave(conv)).await
}
