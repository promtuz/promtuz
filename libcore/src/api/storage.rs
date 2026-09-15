//! Storage inventory carries metadata, never media payloads, across FFI.
use crate::{db::messages::MESSAGES_DB, platform::CoreError};
use crate::events::Emittable;
use super::messaging::{to_conv16, to_did16};

#[derive(uniffi::Record)]
pub struct StoredMedia {
    pub conversation_id: Vec<u8>,
    pub dispatch_id: Vec<u8>,
    pub kind: u8,
    pub name: String,
    pub caption: String,
    pub timestamp: u64,
    /// Logical local bytes. Shared files can appear in more than one message;
    /// SQLite also reuses freed pages rather than immediately shrinking.
    pub local_bytes: u64,
}

#[uniffi::export]
pub fn storage_media() -> Result<Vec<StoredMedia>, CoreError> {
    inventory().map_err(Into::into)
}

fn inventory() -> anyhow::Result<Vec<StoredMedia>> {
    let db = MESSAGES_DB.lock();
    let mut query = db.prepare(
        "SELECT mm.conversation_id, mm.dispatch_id, mm.kind, mm.name,
                substr(m.content, 1, 120), m.timestamp,
                coalesce(length(mm.blob), 0) + coalesce(length(mm.thumb), 0), mm.file_id
         FROM message_media mm JOIN messages m
         ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE m.deleted = 0 AND NOT (m.outgoing = 1 AND m.status IN (0, 2))")?;
    let rows = query.query_map([], |r| Ok((StoredMedia {
        conversation_id: r.get(0)?, dispatch_id: r.get(1)?, kind: r.get(2)?,
        name: r.get(3)?, caption: r.get(4)?, timestamp: r.get(5)?, local_bytes: r.get(6)?,
    }, r.get::<_, Option<Vec<u8>>>(7)?)))?;
    let mut result = Vec::new();
    for row in rows {
        let (mut item, fid) = row?;
        if let Some(fid) = fid.and_then(|f| <[u8; 32]>::try_from(f).ok()) {
            let partial = crate::transfer::store::partial_get(&fid);
            // An in-progress transfer is not a storage-cleanup candidate.
            if partial.as_ref().is_some_and(|p| !p.is_complete()) { continue; }
            let path = partial.map(|p| p.path).or_else(||
                crate::transfer::store::retention_get(&fid).map(|r| r.path));
            if let Some(path) = path {
                item.local_bytes += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            }
        }
        if item.local_bytes > 0 { result.push(item); }
    }
    result.sort_by_key(|m| std::cmp::Reverse(m.local_bytes));
    Ok(result)
}

/// Fetch a single visible row's image preview, never the entire library.
#[uniffi::export]
pub fn storage_preview(conversation_id: Vec<u8>, dispatch_id: Vec<u8>) -> Result<Option<Vec<u8>>, CoreError> {
    use rusqlite::OptionalExtension;
    let conv = to_conv16(&conversation_id)?;
    let did = to_did16(&dispatch_id)?;
    MESSAGES_DB.lock().query_row(
        "SELECT CASE kind WHEN 1 THEN blob WHEN 2 THEN thumb ELSE NULL END
         FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), did.as_slice()), |r| r.get::<_, Option<Vec<u8>>>(0))
        .optional().map(Option::flatten).map_err(|e| anyhow::Error::from(e).into())
}

#[derive(uniffi::Record)]
pub struct StorageTarget {
    pub conversation_id: Vec<u8>,
    pub dispatch_id: Vec<u8>,
}

/// Local deletion only. Finish the transaction before reporting success or
/// unlinking shared attachments. Revalidate against sends/transfers in progress.
#[uniffi::export]
pub fn remove_stored_media(targets: Vec<StorageTarget>) -> Result<u32, CoreError> {
    remove(targets).map_err(Into::into)
}

fn remove(targets: Vec<StorageTarget>) -> anyhow::Result<u32> {
    use crate::db::messages::MessageRow;
    use rusqlite::OptionalExtension;
    let keys = targets.iter().map(|t| Ok((to_conv16(&t.conversation_id)?, to_did16(&t.dispatch_id)?)))
        .collect::<Result<Vec<_>, CoreError>>()?;
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    let mut removed = Vec::new();
    let mut orphans = Vec::new();
    for (conv, did) in keys {
        let row = tx.query_row(
            "SELECT m.* FROM messages m JOIN message_media mm
             ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
             WHERE m.conversation_id = ?1 AND m.dispatch_id = ?2
             AND m.deleted = 0 AND NOT (m.outgoing = 1 AND m.status IN (0, 2))",
            (conv.as_slice(), did.as_slice()), MessageRow::from_row).optional()?;
        let Some(row) = row else { continue; };
        let fid: Option<Vec<u8>> = tx.query_row(
            "SELECT file_id FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), did.as_slice()), |r| r.get(0))?;
        if let Some(fid) = fid.and_then(|f| <[u8; 32]>::try_from(f).ok()) {
            if crate::staging::holds(&fid) || crate::transfer::store::partial_get(&fid)
                .is_some_and(|p| !p.is_complete()) { continue; }
        }
        tx.execute("DELETE FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), did.as_slice()))?;
        if let Some(fid) = crate::data::media::drop_row_tx(&tx, &conv, &did)? { orphans.push(fid); }
        removed.push((conv, row));
    }
    tx.commit()?;
    crate::data::media::unlink_orphaned(&db, &orphans);
    drop(db);
    let count = removed.len() as u32;
    for (conversation, row) in removed {
        crate::events::messaging::MessageEv::Deleted { id: row.id, conversation }.emit();
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::media::{self, MediaRow};

    #[test]
    fn storage_cleanup_preserves_shared_files_and_pending_messages() {
        let dir = std::env::temp_dir().join(format!("promtuz-storage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let conv = [181; 16];
        let other = [182; 16];
        let sender = [183; 32];
        let fid = [184; 32];
        let file = dir.join("shared.bin");
        std::fs::write(&file, [42; 4096]).unwrap();
        crate::transfer::store::retention_put(&fid, file.to_str().unwrap(), 4096, 1024, &[], u64::MAX / 2).unwrap();
        let row = MediaRow { kind: media::KIND_ATTACHMENT, group_id: None, mime: "application/octet-stream".into(),
            name: "shared.bin".into(), size: 4096, width: 0, height: 0, duration_ms: 0,
            blob: None, thumb: Some(vec![1; 8]), file_id: Some(fid.to_vec()) };
        media::save_incoming_with_media(&conv, &sender, &[1; 16], "caption", 10, None, &row).unwrap();
        media::save_incoming_with_media(&other, &sender, &[2; 16], "other caption", 11, None, &row).unwrap();
        let photo = MediaRow { kind: media::KIND_IMAGE, blob: Some(vec![9; 1024]), thumb: None,
            file_id: None, size: 1024, ..row.clone() };
        media::save_incoming_with_media(&conv, &sender, &[3; 16], "photo", 12, None, &photo).unwrap();
        let pending = media::save_outgoing_with_media(&conv, "sending", None, &photo).unwrap();
        let pending_did = pending.inner.dispatch_id.unwrap();
        let inventory = storage_media().unwrap();
        assert_eq!(inventory.iter().filter(|r| r.conversation_id == conv).count(), 2);
        assert_eq!(inventory.iter().find(|r| r.dispatch_id == vec![1; 16]).unwrap().local_bytes, 4104);
        assert_eq!(inventory.iter().find(|r| r.dispatch_id == vec![3; 16]).unwrap().local_bytes, 1024);
        assert_eq!(storage_preview(conv.to_vec(), vec![3; 16]).unwrap().unwrap().len(), 1024);
        assert!(storage_preview(conv.to_vec(), vec![99; 16]).unwrap().is_none());
        let target = |conversation: [u8; 16], dispatch: Vec<u8>| StorageTarget { conversation_id: conversation.to_vec(), dispatch_id: dispatch };
        // Validate the whole request before mutating any row.
        assert!(remove_stored_media(vec![target(conv, vec![1; 16]), target(conv, vec![0])]).is_err());
        assert!(media::get(&conv, &[1; 16]).unwrap().is_some());
        // A transfer can become active after the inventory was shown.
        let mut partial = crate::transfer::store::Partial {
            file_id: fid, source_ipk: sender, total: 4096, chunk_size: 1024,
            manifest: None, have: 1, state: crate::transfer::store::ACTIVE,
            path: file.to_str().unwrap().into(), updated_at: 12,
        };
        crate::transfer::store::partial_put(&partial).unwrap();
        assert_eq!(remove_stored_media(vec![target(conv, vec![1; 16])]).unwrap(), 0);
        assert!(file.exists());
        assert!(!storage_media().unwrap().iter().any(|r| r.dispatch_id == vec![1; 16]));
        partial.state = crate::transfer::store::DONE;
        partial.have = 4;
        crate::transfer::store::partial_put(&partial).unwrap();
        assert_eq!(remove_stored_media(vec![target(conv, vec![1; 16]), target(conv, pending_did.clone())]).unwrap(), 1);
        assert!(file.exists(), "another chat still owns this file");
        assert!(crate::data::message::Message::get_by_dispatch(&conv, &pending_did.clone().try_into().unwrap()).is_some());
        assert_eq!(remove_stored_media(vec![target(other, vec![2; 16]), target(conv, vec![3; 16])]).unwrap(), 2);
        assert!(!file.exists(), "last committed reference releases attachment bytes");
        assert!(media::get(&conv, &[3; 16]).unwrap().is_none());
        assert!(crate::data::message::Message::get_by_dispatch(&conv, &[3; 16]).is_none(), "caption is deleted with media");
        assert_eq!(remove_stored_media(vec![target(conv, vec![3; 16])]).unwrap(), 0, "stale selections are harmless");
    }
}
