//! Messaging exports: send + typed read paths (no CBOR).

use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::message::Message;
use crate::db::messages::MessageRow;
use crate::platform::CoreError;

/// A stored message, projected for the client (`ULID` → String, IPK → bytes).
#[derive(uniffi::Record)]
pub struct MessageRecord {
    pub id: String,
    /// The chat this belongs to — 16 bytes, stable for the conversation's life.
    pub conversation_id: Vec<u8>,
    /// Who wrote it. `None` means us, which is how every outgoing row reads.
    pub sender_ipk: Option<Vec<u8>>,
    pub content: String,
    pub outgoing: bool,
    pub timestamp: u64,
    /// 0 = pending, 1 = sent, 2 = failed, 3 = delivered, 4 = read.
    pub status: u8,
    /// 16-byte shared id — the target for edit/delete. None on legacy rows.
    pub dispatch_id: Option<Vec<u8>>,
    /// Sender edited this message's text.
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
    /// dispatch_id of the quoted message, when this is a reply.
    pub reply_to: Option<Vec<u8>>,
    /// 0 for an ordinary message; otherwise a membership/title change, where
    /// `sender_ipk` is who acted and `content` names the target — a hex IPK
    /// for the membership events, the new title for a rename.
    pub system: u8,
}

/// One emoji reaction, projected for the client. `mine` is `reactor == self`
/// (precomputed so the UI needn't hold its own IPK to render).
#[derive(uniffi::Record)]
pub struct ReactionRecord {
    pub dispatch_id: Vec<u8>,
    pub reactor: Vec<u8>,
    pub emoji: String,
    pub timestamp: u64,
    pub mine: bool,
}

/// Unread incoming count for one conversation — the home-list badge source.
#[derive(uniffi::Record)]
pub struct UnreadCount {
    pub conversation_id: Vec<u8>,
    pub count: u32,
}

/// A conversation, projected for the client — the home list's row source.
#[derive(uniffi::Record)]
pub struct ConversationRecord {
    pub id: Vec<u8>,
    /// 0 = direct (a 1:1 chat), 1 = group.
    pub kind: u8,
    /// Group name as it was actually set — empty until someone names it, and
    /// empty for a direct chat. This is what a rename field edits.
    pub title: String,
    /// What to call this chat on screen. Falls back to the members' names for
    /// an unnamed group, so a group whose name never arrived still reads as
    /// the people in it rather than as "Group".
    pub display_name: String,
    /// Active roster, us included. Two entries for a direct chat.
    pub members: Vec<Vec<u8>>,
    /// The other party of a direct chat, resolved core-side so the client
    /// never needs to hold its own IPK to work out which member isn't it.
    /// `None` for a group, which has no single counterpart.
    pub peer: Option<Vec<u8>>,
    /// The active roster minus ourselves — exactly who a send fans out to.
    /// Same list the client needs for presence and typing, which are
    /// per-person and so can never key off the conversation.
    pub others: Vec<Vec<u8>>,
    /// Whether *we* may change this group's membership. v1 grants that to the
    /// creator alone; resolved here because only core knows our own IPK.
    pub can_manage: bool,
    /// True once an MLS group backs this conversation — i.e. it can send.
    pub has_group: bool,
    /// We are still an active member. False for a group we left or were
    /// removed from, which keeps its history but can no longer send.
    pub am_member: bool,
    /// Leaving is offered. False for a direct chat, which has no membership,
    /// and for a group we already left — and see [`Self::owner_is_stuck`].
    pub can_leave: bool,
    /// Deleting is offered. See [`Self::owner_is_stuck`] for the one case a
    /// group refuses it.
    pub can_delete: bool,
    /// We founded this group and other people are still in it, so both leaving
    /// and deleting are refused: the group would be left with nobody able to
    /// manage it. Lifted by removing everyone first — or, later, by handing
    /// the group to someone else. Carried so the UI can say *why*.
    pub owner_is_stuck: bool,
    pub created_at: u64,
}

/// One member's standing in a conversation.
#[derive(uniffi::Record)]
pub struct MemberRecord {
    pub ipk: Vec<u8>,
    /// 0 = member, 1 = admin. v1 mints exactly one admin: the creator.
    pub role: u8,
    pub joined_at: u64,
    /// False once they left or were removed; their old messages still attribute.
    pub active: bool,
    /// This row is us. Resolved here for the same reason as `ReactionRecord.mine`:
    /// the client would otherwise hold its own IPK just to compare against.
    pub me: bool,
}

/// An address-book entry, projected for the client.
#[derive(uniffi::Record)]
pub struct ContactInfo {
    pub ipk: Vec<u8>,
    pub name: String,
    pub added_at: u64,
    /// Pairing state: 0 = pending, 1 = paired, 2 = rejected (PAIRING.md).
    pub status: u8,
    /// Why rejected (a DECLINE_* code), when status = 2.
    pub reject_reason: Option<u8>,
}

/// Send `content` to `to_ipk`, optionally quoting a prior message by its
/// 16-byte `reply_to` dispatch_id. Fire-and-forget: the outcome arrives via
/// `CoreEvents::on_message` (Sent / Failed), matching the engine's
/// event-driven model. The `Result` only reports invalid input (a bad
/// IPK length) synchronously.
#[uniffi::export]
pub fn send_message(
    conversation_id: Vec<u8>, content: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let reply = reply_to.as_deref().map(to_did16).transpose()?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send(conv, content, reply).await {
            log::error!("MESSAGE: send failed: {e}");
        }
    });
    Ok(())
}

/// Edit a prior message (targets it by its 16-byte `dispatch_id`). Fire-and-
/// forget; the change is applied locally and surfaces via `on_message(Edited)`.
#[uniffi::export]
pub fn edit_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, content: String,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::edit(conv, target, content).await {
            log::error!("MESSAGE: edit failed: {e}");
        }
    });
    Ok(())
}

/// Emit an ephemeral activity signal to `peer` — an OR of `ACTIVITY_*` bits
/// (0 = present-idle). Fire-and-forget; dropped if we or the peer are offline.
/// The peer sees it via `on_activity`. Call on typing start/stop (throttled).
#[uniffi::export]
pub fn set_activity(conversation_id: Vec<u8>, activity: u16) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::set_activity(conv, activity).await {
            log::debug!("MESSAGE: set_activity failed: {e}");
        }
    });
    Ok(())
}

/// Add (`add = true`) or remove our own `emoji` reaction on a message
/// (targeted by 16-byte `dispatch_id`). Fire-and-forget; surfaces via
/// `on_reaction`. A person may stack several distinct emoji on one message.
#[uniffi::export]
pub fn react_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, emoji: String, add: bool,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::react(conv, target, emoji, add).await {
            log::error!("MESSAGE: react failed: {e}");
        }
    });
    Ok(())
}

/// All reactions in a conversation, oldest first. The UI groups by
/// `dispatch_id`; `mine` marks the caller's own.
#[uniffi::export]
pub fn reactions_for(conversation_id: Vec<u8>) -> Result<Vec<ReactionRecord>, CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let me = crate::data::identity::Identity::get().map(|i| i.ipk());
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

/// Tell `peer` we've read their messages up to `upto_dispatch_id` (a 16-byte
/// dispatch id). High-water-mark — one call clears the whole unread backlog.
/// Sends a Read receipt; the peer sees it as a status bump via `on_message`
/// (Receipt). Delivered receipts are automatic on message arrival.
#[uniffi::export]
pub fn mark_read(conversation_id: Vec<u8>, upto_dispatch_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let upto = to_did16(&upto_dispatch_id)?;
    // Persist locally first so the home unread count clears the moment the user
    // reads in-chat (the write rings the reactive doorbell); then tell the others.
    Message::set_read_watermark(&conv, &upto);
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_receipt(
            conv, common::proto::mls_wire::ReceiptKind::Read, upto,
        )
        .await
        {
            log::debug!("MESSAGE: mark_read failed: {e}");
        }
    });
    Ok(())
}

/// Mark the whole conversation with `peer` read: advance the local watermark to
/// the newest incoming message and send a Read receipt. No-op if nothing's
/// incoming. For the home-list "Mark read" action, where the caller has no
/// specific dispatch id in hand.
#[uniffi::export]
pub fn mark_conversation_read(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let Some(upto) = Message::newest_incoming_dispatch(&conv) else { return Ok(()) };
    Message::set_read_watermark(&conv, &upto);
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_receipt(
            conv, common::proto::mls_wire::ReceiptKind::Read, upto,
        )
        .await
        {
            log::debug!("MESSAGE: mark_conversation_read failed: {e}");
        }
    });
    Ok(())
}

/// Unread incoming count per peer (only peers with unread > 0). Home-list badges.
#[uniffi::export]
pub fn unread_counts() -> Vec<UnreadCount> {
    Message::unread_counts()
        .into_iter()
        .map(|(conv, count)| UnreadCount { conversation_id: conv.to_vec(), count })
        .collect()
}

/// Subscribe to presence for `contacts` (replaces the prior interest set).
/// Fire-and-forget; a contact's presence surfaces via `on_presence` only when
/// they've also subscribed to us. Call on connect and when contacts change.
#[uniffi::export]
pub fn subscribe_presence(contacts: Vec<Vec<u8>>) -> Result<(), CoreError> {
    let list = contacts.iter().map(|c| to_ipk32(c)).collect::<Result<Vec<_>, _>>()?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::subscribe_presence(list).await {
            log::debug!("PRESENCE: subscribe failed: {e}");
        }
    });
    Ok(())
}

/// Set our activity mode: `idle = true` on backgrounding, `false` on
/// foreground. Fire-and-forget; contacts see us go idle/active (PRESENCE.md).
#[uniffi::export]
pub fn set_presence(idle: bool) {
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::set_presence(idle).await {
            log::debug!("PRESENCE: set_presence failed: {e}");
        }
    });
}

/// (Re)register our push-pseudonym with the connected home relay so it can
/// wake us on offline delivery. Fire-and-forget; also runs automatically on
/// each connect. Call after obtaining/refreshing the platform push token.
#[uniffi::export]
pub fn register_push() {
    crate::RUNTIME.spawn(async {
        if let Err(e) = crate::push::register_push().await {
            log::debug!("PUSH: register failed: {e}");
        }
    });
}

/// Provide/refresh the platform push token — call from the FCM `onNewToken`
/// callback. Stores it and registers `P → token` with a gateway so a wake can
/// reach this device.
#[uniffi::export]
pub fn register_push_token(token: Vec<u8>) {
    crate::RUNTIME.spawn(async move {
        crate::push::set_push_token(token).await;
    });
}

/// Delete a prior message. `for_everyone` tombstones both sides; otherwise it's
/// a local-only removal. Surfaces via `on_message(Deleted)`.
#[uniffi::export]
pub fn delete_message(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, for_everyone: bool,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::delete(conv, target, for_everyone).await {
            log::error!("MESSAGE: delete failed: {e}");
        }
    });
    Ok(())
}

/// Paginated history for a conversation, oldest-first. `before_id` (a ULID)
/// pages backwards; pass an empty string for the latest page.
#[uniffi::export]
pub fn get_messages(
    conversation_id: Vec<u8>, limit: u32, before_id: String,
) -> Result<Vec<MessageRecord>, CoreError> {
    let conv = to_conv16(&conversation_id)?;
    Ok(Message::get_messages(&conv, limit, &before_id).into_iter().map(Into::into).collect())
}

/// Every conversation, most recently active first — the home list.
#[uniffi::export]
pub fn list_conversations() -> Vec<ConversationRecord> {
    Conversation::list()
        .into_iter()
        .map(conversation_record)
        .collect()
}

/// Shared projection so the list and single-fetch reads can't drift.
fn conversation_record(c: crate::db::messages::ConversationRow) -> ConversationRecord {
    let others = Conversation::recipients(&c.id);
    let me = crate::data::identity::Identity::get().map(|i| i.ipk());
    let roster = Conversation::members(&c.id);
    let is_group = c.kind == crate::data::conversation::KIND_GROUP;
    let am_member = me.is_some_and(|k| roster.iter().any(|m| m.active && m.member_ipk == k));
    let can_manage = me.is_some_and(|k| Conversation::is_admin(&c.id, &k));
    let owner_is_stuck = is_group && can_manage && am_member && !others.is_empty();

    ConversationRecord {
        members: roster
            .iter()
            .filter(|m| m.active)
            .map(|m| m.member_ipk.to_vec())
            .collect(),
        peer:           Conversation::peer_of(&c.id).map(|p| p.to_vec()),
        can_manage,
        has_group:      c.mls_group_id.is_some(),
        am_member,
        can_leave:      is_group && am_member && !owner_is_stuck,
        can_delete:     !owner_is_stuck,
        owner_is_stuck,
        display_name:   display_name(&c, &others),
        others:         others.into_iter().map(|p| p.to_vec()).collect(),
        id:             c.id.to_vec(),
        kind:           c.kind,
        title:          c.title,
        created_at:     c.created_at,
    }
}

/// What to call a conversation on screen.
///
/// A group's name can legitimately be missing — we were Welcomed into it before
/// anyone told us what it's called, or that message was lost — so fall back to
/// the people in it. That reads as the chat it is instead of as "Group", and it
/// needs nothing to arrive over the network to be right.
fn display_name(c: &crate::db::messages::ConversationRow, others: &[[u8; 32]]) -> String {
    if !c.title.is_empty() {
        return c.title.clone();
    }
    let mut names: Vec<String> = others
        .iter()
        .map(|ipk| {
            crate::data::contact::Contact::get(ipk)
                .map(|c| c.inner.name.clone())
                .unwrap_or_else(|| hex::encode(&ipk[..4]))
        })
        .collect();
    names.sort();
    match names.len() {
        0 => String::new(),
        1..=3 => names.join(", "),
        // Past three the list stops being a name and starts being a roster.
        _ => format!("{}, {} and {} more", names[0], names[1], names.len() - 2),
    }
}

/// Drop a conversation and its history from this device.
///
/// Local only, and deliberately not a membership change: nobody else is told,
/// and a group you are still in comes back the moment someone posts in it —
/// the MLS group lives in openmls storage, which this doesn't touch, so the
/// next message re-opens the chat from the group's own roster and name. That
/// is also why leaving is a separate action: this one does not take you out.
///
/// Once you are no longer a member, nothing can arrive, so the MLS state is
/// dropped along with it rather than lingering forever.
///
/// Refused for a group you founded while others are still in it — see
/// [`crate::groups::require_not_stranding_the_group`].
#[uniffi::export]
pub fn delete_conversation(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let Some(row) = Conversation::get(&conv) else { return Ok(()) };
    let me = crate::data::identity::Identity::get().map(|i| i.ipk());

    if let Some(me) = me {
        crate::groups::require_not_stranding_the_group(&conv, &me)?;
    }
    let still_a_member = me.is_some_and(|k| {
        Conversation::members(&conv).iter().any(|m| m.active && m.member_ipk == k)
    });

    if !still_a_member {
        if let Some(gid) = Conversation::group_of(&conv) {
            let provider = crate::mls::PromtuzMlsProvider::shared();
            match crate::mls::MlsGroupHandle::load(&provider, &gid) {
                Ok(Some(mut g)) =>
                    if let Err(e) = g.delete(&provider) {
                        log::warn!("DELETE: dropping MLS state failed: {e}");
                    },
                Ok(None) => {},
                Err(e) => log::warn!("DELETE: loading MLS state failed: {e}"),
            }
            let buffer = crate::mls::EpochCatchupBuffer::new(crate::db::mls::stash_db_handle());
            if let Err(e) = buffer.purge_group(&gid) {
                log::warn!("DELETE: epoch buffer purge failed: {e}");
            }
        }
    }
    Conversation::delete(&conv)?;
    log::info!(
        "DELETE: dropped conversation {} ({})",
        hex::encode(&conv[..4]),
        if row.kind == crate::data::conversation::KIND_GROUP { "group" } else { "direct" }
    );
    Ok(())
}

/// The direct conversation with `peer_ipk`, created if this is the first time
/// it's been opened. How the contacts list turns a person into a chat.
#[uniffi::export]
pub fn conversation_with(peer_ipk: Vec<u8>) -> Result<Vec<u8>, CoreError> {
    let peer = to_ipk32(&peer_ipk)?;
    Ok(Conversation::for_peer(&peer)?.to_vec())
}

/// One conversation by id, or `None` if it's gone.
#[uniffi::export]
pub fn get_conversation(conversation_id: Vec<u8>) -> Result<Option<ConversationRecord>, CoreError> {
    let conv = to_conv16(&conversation_id)?;
    Ok(Conversation::get(&conv).map(conversation_record))
}

/// Full roster including departed members, so historic messages still
/// attribute to a name.
#[uniffi::export]
pub fn conversation_members(conversation_id: Vec<u8>) -> Result<Vec<MemberRecord>, CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let me = crate::data::identity::Identity::get().map(|i| i.ipk());
    Ok(Conversation::members(&conv)
        .into_iter()
        .map(|m| MemberRecord {
            me:        me.is_some_and(|k| k == m.member_ipk),
            ipk:       m.member_ipk.to_vec(),
            role:      m.role,
            joined_at: m.joined_at,
            active:    m.active,
        })
        .collect())
}

/// How many members have read up to `dispatch_id` — the "seen by N" figure.
#[uniffi::export]
pub fn seen_by_count(conversation_id: Vec<u8>, dispatch_id: Vec<u8>) -> Result<u32, CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let did = to_did16(&dispatch_id)?;
    Ok(Message::seen_by_count(&conv, &did))
}

/// Rename a conversation. Applied locally at once so the UI doesn't wait on
/// the network; a group's new name is then narrated to its members, who apply
/// it on receipt. A direct chat's title is ours alone, so it stays local.
#[uniffi::export]
pub fn set_conversation_title(
    conversation_id: Vec<u8>, title: String,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    Conversation::set_title(&conv, &title)?;
    let is_group = Conversation::get(&conv)
        .is_some_and(|c| c.kind == crate::data::conversation::KIND_GROUP);
    if is_group {
        crate::RUNTIME.spawn(async move {
            crate::messaging::announce(
                conv,
                common::proto::mls_wire::SystemEvent::Titled { title },
            )
            .await;
        });
    }
    Ok(())
}

/// One entry per conversation (latest message per peer).
#[uniffi::export]
pub fn get_conversations() -> Vec<MessageRecord> {
    Message::get_conversations().into_iter().map(Into::into).collect()
}

/// All contacts, newest first.
#[uniffi::export]
pub fn get_contacts() -> Vec<ContactInfo> {
    Contact::list()
        .into_iter()
        .map(|c| ContactInfo {
            ipk: c.ipk.to_vec(),
            name: c.name,
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
    /// True once an MLS group id is bound (first send has happened).
    pub paired: bool,
    /// Current MLS epoch, `None` if unpaired or the group can't load.
    pub epoch: Option<u64>,
    pub message_count: u32,
    /// Newest message status (0 pending / 1 sent / 2 failed), `None` if none.
    pub last_status: Option<u8>,
    /// Pending (undelivered) outbox ops for this peer.
    pub pending_ops: u32,
}

/// Cascade-delete ALL per-contact state so re-scanning this peer's QR is a
/// clean first-time add: MLS group storage, epoch-ahead buffer, messages,
/// queued outbox ops, then the address-book row (last, after its group id
/// is consumed). Best-effort — a failing store is logged and the cascade
/// continues; partial cleanup beats aborting on stale state. Idempotent:
/// forgetting an absent contact is success.
#[uniffi::export]
pub fn forget_contact(ipk: Vec<u8>) -> Result<(), CoreError> {
    let ipk = to_ipk32(&ipk)?;
    let Some(contact) = Contact::get(&ipk) else { return Ok(()) };

    if let Some(gid) = contact.inner.mls_group_id {
        let provider = crate::mls::PromtuzMlsProvider::shared();
        match crate::mls::MlsGroupHandle::load(&provider, &gid) {
            Ok(Some(mut g)) =>
                if let Err(e) = g.delete(&provider) {
                    log::error!("FORGET: mls group delete failed: {e}");
                },
            Ok(None) => {},
            Err(e) => log::error!("FORGET: mls group load failed: {e}"),
        }
        let buffer = crate::mls::EpochCatchupBuffer::new(crate::db::mls::stash_db_handle());
        if let Err(e) = buffer.purge_group(&gid) {
            log::error!("FORGET: epoch buffer purge failed: {e}");
        }
    }

    if let Ok(conv) = Conversation::for_peer(&ipk) {
        if let Err(e) = Conversation::delete(&conv) {
            log::error!("FORGET: conversation delete failed: {e}");
        }
    }
    crate::delivery::forget_target(&ipk);
    // Sever any live direct link so a forgotten contact can't keep talking
    // over an already-open P2P connection.
    crate::p2p::drop_link(&ipk);
    if let Err(e) = Contact::delete(&ipk) {
        log::error!("FORGET: contact delete failed: {e}");
    }
    Ok(())
}

/// Contacts list enriched with per-contact diagnostics for a debug UI.
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
                name: c.name,
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
        }
    }
}

/// Validate a client-supplied IPK is exactly 32 bytes.
pub(crate) fn to_ipk32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "ipk must be 32 bytes".into() })
}

/// Validate a client-supplied conversation id is exactly 16 bytes.
pub(crate) fn to_conv16(bytes: &[u8]) -> Result<[u8; 16], CoreError> {
    bytes
        .try_into()
        .map_err(|_| CoreError::Internal { msg: "conversation id must be 16 bytes".into() })
}

/// Validate a client-supplied dispatch_id is exactly 16 bytes.
pub(crate) fn to_did16(bytes: &[u8]) -> Result<[u8; 16], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "dispatch_id must be 16 bytes".into() })
}

/// Validate a client-supplied file_id is exactly 32 bytes.
pub(crate) fn to_fid32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "file_id must be 32 bytes".into() })
}

// ── Group membership ──────────────────────────────────────────────────────
//
// All four need a live relay (a KeyPackage fetch and a Welcome), so unlike a
// message they report their outcome synchronously rather than outboxing.
//
// These are the only `async` exports on the surface, and uniffi polls them on
// its own executor — no Tokio reactor in scope, so QUIC I/O inside would fail
// with "there is no reactor running". [`on_runtime`] moves the work onto the
// global runtime; the JoinHandle we await back is a plain future the runtime
// wakes, so uniffi's executor is fine holding it.

/// Run `fut` on [`crate::RUNTIME`] and await its result.
async fn on_runtime<T, F>(fut: F) -> Result<T, CoreError>
where
    T: Send + 'static,
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
{
    crate::RUNTIME
        .spawn(fut)
        .await
        .map_err(|e| CoreError::Internal { msg: format!("group task did not finish: {e}") })?
        .map_err(CoreError::from)
}

/// Create a group with `members` and us as its admin. Returns the new
/// conversation id, ready to send in.
#[uniffi::export]
pub async fn create_group(title: String, members: Vec<Vec<u8>>) -> Result<Vec<u8>, CoreError> {
    let list = members.iter().map(|m| to_ipk32(m)).collect::<Result<Vec<_>, _>>()?;
    let id = on_runtime(crate::groups::create_group(title, list)).await?;
    Ok(id.to_vec())
}

/// Add someone to a group. Admin-only; they get no pre-join history.
#[uniffi::export]
pub async fn add_group_member(
    conversation_id: Vec<u8>, member_ipk: Vec<u8>,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let who = to_ipk32(&member_ipk)?;
    on_runtime(crate::groups::add_member(conv, who)).await
}

/// Remove someone from a group, rotating keys afterwards so their device can't
/// read what follows. Admin-only.
#[uniffi::export]
pub async fn remove_group_member(
    conversation_id: Vec<u8>, member_ipk: Vec<u8>,
) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    let who = to_ipk32(&member_ipk)?;
    on_runtime(crate::groups::remove_member(conv, who)).await
}

/// Leave a group. The conversation and its history stay; it just can't send.
#[uniffi::export]
pub async fn leave_group(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = to_conv16(&conversation_id)?;
    on_runtime(crate::groups::leave(conv)).await
}
