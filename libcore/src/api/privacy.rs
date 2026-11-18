//! Privacy commands are enforced in core; a route or external intent cannot unlock history.
use crate::{platform::CoreError, state::core};
use common::types::bytes::fixed;
use zeroize::Zeroizing;
#[uniffi::export]
pub fn lock_conversation(conversation_id: Vec<u8>, code: String) -> Result<(), CoreError> {
    let code = Zeroizing::new(code);
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let record = crate::data::conversation::Conversation::get(&conv)
        .ok_or_else(|| anyhow::anyhow!("Chat not found"))?;
    if record.mls_group_id.is_none() || crate::requests::is_request_chat(&conv) {
        return Err(anyhow::anyhow!("Only established chats can be locked").into());
    }
    if crate::call::current().is_some_and(|call| call.conversation == conv) {
        return Err(anyhow::anyhow!("End the call before locking this chat").into());
    }
    crate::privacy::lock(&conv, &code)?;
    Ok(())
}
#[derive(uniffi::Record)]
pub struct HiddenTag {
    pub code: String,
    /// An index into the client's tag palette.
    pub color: u8,
}
impl From<crate::hidden::Tag> for HiddenTag {
    fn from(t: crate::hidden::Tag) -> Self {
        Self { code: t.code, color: t.color }
    }
}
/// Locks the chat first when `code` is given; otherwise it must be locked and open.
#[uniffi::export]
pub fn hide_conversation(conversation_id: Vec<u8>, code: Option<String>) -> Result<HiddenTag, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    match code {
        Some(code) => {
            let code = Zeroizing::new(code);
            let generation = private_generation();
            lock_conversation(conversation_id, code.to_string())?;
            // The person just chose the code, so the chat stays open as if they had entered it.
            crate::privacy::unlock_at(&code, generation)?;
        },
        None if !core().private_sessions.read().contains_key(&conv) => {
            return Err(anyhow::anyhow!("Chat is locked").into());
        },
        None => {},
    }
    Ok(crate::hidden::hide(&conv)?.into())
}
#[uniffi::export]
pub fn unhide_conversation(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    crate::hidden::unhide(&conv)?;
    Ok(())
}
/// Shown only while the chat is open.
#[uniffi::export]
pub fn hidden_tag(conversation_id: Vec<u8>) -> Option<HiddenTag> {
    let conv = fixed::<16>(&conversation_id, "conversation id").ok()?;
    if !core().private_sessions.read().contains_key(&conv) {
        return None;
    }
    crate::hidden::tag(&conv).map(Into::into)
}
#[uniffi::export]
pub fn unlock_private_conversation(code: String, generation: u64) -> Result<Option<Vec<u8>>, CoreError> {
    let code = Zeroizing::new(code);
    let conversation = crate::privacy::unlock_at(&code, generation)?;
    if let Some(conv) = conversation {
        core().spawn(async move {
            if let Err(error) = crate::mls::privacy::drain(conv).await {
                log::warn!("PRIVATE: buffered inbox remains pending: {error:#}");
            }
        });
    }
    Ok(conversation.map(|c| c.to_vec()))
}
/// Read before deciding to unlock; any lock or cancel since then refuses the unlock.
#[uniffi::export]
pub fn private_generation() -> u64 {
    core().private_generation.load(std::sync::atomic::Ordering::SeqCst)
}
/// Refuses unlocks already in flight without closing an open chat.
#[uniffi::export]
pub fn cancel_private_unlocks() {
    let _sessions = core().private_sessions.write();
    core().private_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}
#[uniffi::export]
pub fn lock_private_conversations() {
    {
        let _operations = core().mls_operations.iter().map(|lock| lock.lock()).collect::<Vec<_>>();
        let mut sessions = core().private_sessions.write();
        core().private_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        sessions.clear();
    }
    crate::staging::lock_private();
    // After the operation locks: sealing takes the configuration lock, which ranks before them.
    crate::hidden::seal_all();
}
#[uniffi::export]
pub fn remove_conversation_lock(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    crate::privacy::remove_lock(&conv)?;
    Ok(())
}
#[uniffi::export]
pub fn private_conversation_ids() -> Result<Vec<Vec<u8>>, CoreError> {
    Ok(crate::privacy::ids_tx(&core().db.messages().lock())?
        .into_iter()
        .map(|c| c.to_vec())
        .collect())
}
/// Whether the chat's code is a 6-digit PIN, so the code screen can show a keypad while locked.
#[uniffi::export]
pub fn private_code_is_pin(conversation_id: Vec<u8>) -> bool {
    fixed::<16>(&conversation_id, "conversation id").is_ok_and(|conv| crate::privacy::uses_pin(&conv))
}
#[uniffi::export]
pub fn is_private_conversation_unlocked(conversation_id: Vec<u8>) -> bool {
    fixed::<16>(&conversation_id, "conversation id")
        .ok()
        .is_some_and(|c| core().private_sessions.read().contains_key(&c))
}
/// Scoped unread metadata for a code-unlocked chat. Global counts keep private chats excluded.
#[uniffi::export]
pub fn private_conversation_unread_count(conversation_id: Vec<u8>) -> Result<u32, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let conn = core().db.messages().lock();
    let sessions = core().private_sessions.read();
    if !crate::privacy::is_private_tx(&conn, &conv)? || !sessions.contains_key(&conv) {
        return Err(anyhow::anyhow!("Chat is locked").into());
    }
    let count = conn.query_row(
        "SELECT COUNT(*) FROM incoming_receipts r JOIN messages m ON m.id=r.message_id
         WHERE m.conversation_id=?1 AND r.is_read=0 AND m.deleted=0 AND m.system=0",
        [conv],
        |row| row.get::<_, u32>(0),
    ).map_err(anyhow::Error::from)?;
    Ok(count)
}
/// Deliberate privacy-settings entry for destructive reset. This exposes only current contact
/// identities, never a preview, historical metadata, or an unlocked history capability.
#[derive(uniffi::Record)]
pub struct PrivateResetOption {
    pub conversation_id: Vec<u8>,
    pub name: String,
    pub is_group: bool,
    /// A hidden chat is listed by its tag, never by name.
    pub tag: Option<HiddenTag>,
    /// Unix seconds; 0 for a lock made before this was recorded.
    pub locked_at: u64,
}
#[uniffi::export]
pub fn locked_conversation_reset_options() -> Result<Vec<PrivateResetOption>, CoreError> {
    use crate::data::conversation::{Conversation, KIND_GROUP};
    let ids = crate::privacy::ids_tx(&core().db.messages().lock())?;
    let locked_at = |id: &[u8; 16]| -> u64 {
        core().db.messages().lock()
            .query_row("SELECT locked_at FROM private_conversations WHERE conversation_id=?1", [id], |r| r.get::<_, Option<u64>>(0))
            .ok().flatten().unwrap_or(0)
    };
    Ok(ids.into_iter().filter_map(|id| {
        if let Some(tag) = crate::hidden::tag(&id) {
            return Some(PrivateResetOption { conversation_id: id.to_vec(), name: String::new(), is_group: false, tag: Some(tag.into()), locked_at: locked_at(&id) });
        }
        let record = Conversation::get(&id)?;
        let is_group = record.kind == KIND_GROUP;
        let name = if is_group && !record.title.is_empty() {
            record.title
        } else {
            let mut names = Conversation::recipients(&id).iter()
                .map(crate::data::peer_name::resolve).collect::<Vec<_>>();
            names.sort();
            if names.is_empty() { if is_group { "Group".into() } else { "Contact".into() } }
            else { names.join(", ") }
        };
        Some(PrivateResetOption { conversation_id: id.to_vec(), name, is_group, tag: None, locked_at: locked_at(&id) })
    }).collect())
}
#[uniffi::export]
pub fn reset_private_conversation(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let _configuration = core().private_configuration.lock();
    // Validate before touching transport state: this command cannot reset an ordinary chat.
    if !crate::privacy::is_private(&conv) {
        return Err(anyhow::anyhow!("Chat is not locked").into());
    }
    let _operations = core().mls_operations.iter().map(|lock| lock.lock()).collect::<Vec<_>>();
    {
        let mut sessions = core().private_sessions.write();
        core().private_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        sessions.remove(&conv);
    }
    // A hidden person goes with the code: seal any restored rows so the reset discards them.
    let hidden = crate::hidden::is_hidden(&conv);
    if hidden {
        crate::hidden::seal_held(&conv)?;
    }
    {
        let mls = core().db.mls();
        let mut mls = mls.lock();
        let groups = crate::db::all(
            &mls,
            "SELECT group_id FROM mls_private_conversations WHERE conversation_id=?1",
            [conv],
            |r| r.get::<_, [u8; 32]>(0),
        )
        .map_err(anyhow::Error::from)?;
        let tx = mls.transaction().map_err(anyhow::Error::from)?;
        for gid in groups {
            crate::mls::privacy::clear_private_group_tx(&tx, &gid)?;
        }
        tx.commit().map_err(anyhow::Error::from)?;
    }
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction().map_err(anyhow::Error::from)?;
    if !crate::privacy::is_private_tx(&tx, &conv)? {
        return Err(anyhow::anyhow!("Chat is not locked").into());
    }
    let orphaned = crate::data::conversation::Conversation::clear_history_tx(&tx, &conv)?;
    let private_files = crate::private_files::clear_conversation_tx(&tx, &conv)?;
    let is_group = crate::data::conversation::Conversation::get_tx(&tx, &conv)
        .is_some_and(|c| c.kind == crate::data::conversation::KIND_GROUP);
    // Group identity and roster are the equivalent of keeping a direct contact. They are
    // required to authenticate a fresh member-owned KeyPackage recovery, never old traffic.
    if hidden {
        tx.execute("DELETE FROM conversations WHERE id=?1", [conv]).map_err(anyhow::Error::from)?;
    } else if !is_group {
        tx.execute("UPDATE conversations SET mls_group_id=NULL WHERE id=?1", [conv])
            .map_err(anyhow::Error::from)?;
    }
    tx.execute("DELETE FROM group_pictures WHERE conversation_id=?1", [conv])
        .map_err(anyhow::Error::from)?;
    tx.execute("DELETE FROM app_prefs WHERE key=?1", [format!("group_pending:{}", hex::encode(conv))])
        .map_err(anyhow::Error::from)?;
    tx.execute("DELETE FROM private_conversations WHERE conversation_id=?1", [conv])
        .map_err(anyhow::Error::from)?;
    tx.commit().map_err(anyhow::Error::from)?;
    crate::data::media::unlink_orphaned(core(), &conn, &orphaned);
    for path in private_files { let _ = std::fs::remove_file(path); }
    drop(conn);
    crate::data::peer_avatar::notify_changed();
    let mut sessions = core().private_sessions.write();
    core().private_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    sessions.remove(&conv);
    drop(sessions);
    crate::staging::lock_private();
    // A reset has no decryption fallback. Recovery is available only through another current
    // member carrying our signed refresh onto the same anchored group's authenticated history.
    if is_group && let Some(gid) = crate::data::conversation::Conversation::group_of(&conv) {
        crate::groups::member_requests::refreshed(&gid)?;
        if crate::groups::member_requests::known_anchor(&gid).is_some()
            && crate::data::identity::Identity::local_ipk().is_some()
        {
            crate::groups::member_requests::refresh(conv)?;
        }
    }
    Ok(())
}
