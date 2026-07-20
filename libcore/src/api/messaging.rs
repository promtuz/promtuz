//! Messaging exports: send + typed read paths (no CBOR).

use crate::data::contact::Contact;
use crate::data::message::Message;
use crate::db::messages::MessageRow;
use crate::platform::CoreError;

/// A stored message, projected for the client (`ULID` → String, IPK → bytes).
#[derive(uniffi::Record)]
pub struct MessageRecord {
    pub id: String,
    pub peer_ipk: Vec<u8>,
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
    pub peer_ipk: Vec<u8>,
    pub count: u32,
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
    to_ipk: Vec<u8>, content: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = to_ipk32(&to_ipk)?;
    let reply = reply_to.as_deref().map(to_did16).transpose()?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send(to, content, reply).await {
            log::error!("MESSAGE: send failed: {e}");
        }
    });
    Ok(())
}

/// Edit a prior message (targets it by its 16-byte `dispatch_id`). Fire-and-
/// forget; the change is applied locally and surfaces via `on_message(Edited)`.
#[uniffi::export]
pub fn edit_message(peer_ipk: Vec<u8>, dispatch_id: Vec<u8>, content: String) -> Result<(), CoreError> {
    let to = to_ipk32(&peer_ipk)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::edit(to, target, content).await {
            log::error!("MESSAGE: edit failed: {e}");
        }
    });
    Ok(())
}

/// Emit an ephemeral activity signal to `peer` — an OR of `ACTIVITY_*` bits
/// (0 = present-idle). Fire-and-forget; dropped if we or the peer are offline.
/// The peer sees it via `on_activity`. Call on typing start/stop (throttled).
#[uniffi::export]
pub fn set_activity(peer_ipk: Vec<u8>, activity: u16) -> Result<(), CoreError> {
    let to = to_ipk32(&peer_ipk)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::set_activity(to, activity).await {
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
    peer_ipk: Vec<u8>, dispatch_id: Vec<u8>, emoji: String, add: bool,
) -> Result<(), CoreError> {
    let to = to_ipk32(&peer_ipk)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::react(to, target, emoji, add).await {
            log::error!("MESSAGE: react failed: {e}");
        }
    });
    Ok(())
}

/// All reactions in a conversation, oldest first. The UI groups by
/// `dispatch_id`; `mine` marks the caller's own.
#[uniffi::export]
pub fn reactions_for(peer_ipk: Vec<u8>) -> Result<Vec<ReactionRecord>, CoreError> {
    let peer = to_ipk32(&peer_ipk)?;
    let me = crate::data::identity::Identity::get().map(|i| i.ipk());
    Ok(crate::data::reaction::Reaction::for_peer(&peer)
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
pub fn mark_read(peer_ipk: Vec<u8>, upto_dispatch_id: Vec<u8>) -> Result<(), CoreError> {
    let to = to_ipk32(&peer_ipk)?;
    let upto = to_did16(&upto_dispatch_id)?;
    // Persist locally first so the home unread count clears the moment the user
    // reads in-chat (the write rings the reactive doorbell); then tell the peer.
    Message::set_read_watermark(&to, &upto);
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_receipt(
            to, common::proto::mls_wire::ReceiptKind::Read, upto,
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
pub fn mark_conversation_read(peer_ipk: Vec<u8>) -> Result<(), CoreError> {
    let peer = to_ipk32(&peer_ipk)?;
    let Some(upto) = Message::newest_incoming_dispatch(&peer) else { return Ok(()) };
    Message::set_read_watermark(&peer, &upto);
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_receipt(
            peer, common::proto::mls_wire::ReceiptKind::Read, upto,
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
        .map(|(peer, count)| UnreadCount { peer_ipk: peer.to_vec(), count })
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
    peer_ipk: Vec<u8>, dispatch_id: Vec<u8>, for_everyone: bool,
) -> Result<(), CoreError> {
    let to = to_ipk32(&peer_ipk)?;
    let target = to_did16(&dispatch_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::delete(to, target, for_everyone).await {
            log::error!("MESSAGE: delete failed: {e}");
        }
    });
    Ok(())
}

/// Paginated history with `peer_ipk`, oldest-first. `before_id` (a ULID)
/// pages backwards; pass an empty string for the latest page.
#[uniffi::export]
pub fn get_messages(
    peer_ipk: Vec<u8>, limit: u32, before_id: String,
) -> Result<Vec<MessageRecord>, CoreError> {
    let peer = to_ipk32(&peer_ipk)?;
    Ok(Message::get_messages(&peer, limit, &before_id).into_iter().map(Into::into).collect())
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

    Message::delete_by_peer(&ipk);
    crate::data::reaction::Reaction::delete_by_peer(&ipk);
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
                message_count: Message::count_by_peer(&c.ipk),
                last_status: Message::last_status_by_peer(&c.ipk),
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
            peer_ipk: r.peer_ipk.to_vec(),
            content: r.content,
            outgoing: r.outgoing,
            timestamp: r.timestamp,
            status: r.status,
            dispatch_id: r.dispatch_id,
            edited: r.edited,
            deleted: r.deleted,
            reply_to: r.reply_to,
        }
    }
}

/// Validate a client-supplied IPK is exactly 32 bytes.
pub(crate) fn to_ipk32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "ipk must be 32 bytes".into() })
}

/// Validate a client-supplied dispatch_id is exactly 16 bytes.
pub(crate) fn to_did16(bytes: &[u8]) -> Result<[u8; 16], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "dispatch_id must be 16 bytes".into() })
}

/// Validate a client-supplied file_id is exactly 32 bytes.
pub(crate) fn to_fid32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "file_id must be 32 bytes".into() })
}
