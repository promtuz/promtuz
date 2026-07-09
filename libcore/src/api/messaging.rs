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
    /// 0 = pending, 1 = sent, 2 = failed.
    pub status: u8,
    /// 16-byte shared id — the target for edit/delete. None on legacy rows.
    pub dispatch_id: Option<Vec<u8>>,
    /// Sender edited this message's text.
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
}

/// An address-book entry, projected for the client.
#[derive(uniffi::Record)]
pub struct ContactInfo {
    pub ipk: Vec<u8>,
    pub name: String,
    pub added_at: u64,
}

/// Send `content` to `to_ipk`. Fire-and-forget: the outcome arrives via
/// `CoreEvents::on_message` (Sent / Failed), matching the engine's
/// event-driven model. The `Result` only reports invalid input (a bad
/// IPK length) synchronously.
#[uniffi::export]
pub fn send_message(to_ipk: Vec<u8>, content: String) -> Result<(), CoreError> {
    let to = to_ipk32(&to_ipk)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send(to, content).await {
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
        .map(|c| ContactInfo { ipk: c.ipk.to_vec(), name: c.name, added_at: c.added_at })
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
    crate::delivery::forget_target(&ipk);
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
        }
    }
}

/// Validate a client-supplied IPK is exactly 32 bytes.
fn to_ipk32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "ipk must be 32 bytes".into() })
}

/// Validate a client-supplied dispatch_id is exactly 16 bytes.
fn to_did16(bytes: &[u8]) -> Result<[u8; 16], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "dispatch_id must be 16 bytes".into() })
}
