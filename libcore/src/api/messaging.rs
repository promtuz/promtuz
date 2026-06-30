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

impl From<MessageRow> for MessageRecord {
    fn from(r: MessageRow) -> Self {
        MessageRecord {
            id: r.id.to_string(),
            peer_ipk: r.peer_ipk.to_vec(),
            content: r.content,
            outgoing: r.outgoing,
            timestamp: r.timestamp,
            status: r.status,
        }
    }
}

/// Validate a client-supplied IPK is exactly 32 bytes.
fn to_ipk32(bytes: &[u8]) -> Result<[u8; 32], CoreError> {
    bytes.try_into().map_err(|_| CoreError::Internal { msg: "ipk must be 32 bytes".into() })
}
