//! Storage inventory carries metadata, never media payloads, across FFI.
use common::types::bytes::fixed;

use crate::db::{all, one};
use crate::events::Emittable;
use crate::platform::CoreError;
use crate::state::Core;
use crate::state::core;
use crate::transfer::store;

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
    inventory(core()).map_err(Into::into)
}

fn inventory(c: &Core) -> anyhow::Result<Vec<StoredMedia>> {
    let db = c.db.messages().lock();
    let rows = all(&db,
        "SELECT mm.conversation_id, mm.dispatch_id, mm.kind, mm.name,
                substr(m.content, 1, 120), m.timestamp,
                coalesce(length(mm.blob), 0) + coalesce(length(mm.thumb), 0), mm.file_id
         FROM message_media mm JOIN messages m
         ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE m.deleted = 0 AND NOT (m.outgoing = 1 AND m.status IN (0, 2))",
        [], |r| Ok((StoredMedia {
            conversation_id: r.get(0)?, dispatch_id: r.get(1)?, kind: r.get(2)?,
            name: r.get(3)?, caption: r.get(4)?, timestamp: r.get(5)?, local_bytes: r.get(6)?,
        }, r.get::<_, Option<Vec<u8>>>(7)?)))?;
    let mut result = Vec::new();
    for (mut item, fid) in rows {
        if let Some(fid) = fid.and_then(|f| <[u8; 32]>::try_from(f).ok()) {
            let transfers = c.db.transfers().lock();
            let partial = store::partial_get_tx(&transfers, &fid);
            // An in-progress transfer is not a storage-cleanup candidate.
            if partial.as_ref().is_some_and(|p| !p.is_complete()) { continue; }
            let path = partial.map(|p| p.path).or_else(||
                store::retention_get_tx(&transfers, &fid).map(|r| r.path));
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
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let did = fixed::<16>(&dispatch_id, "dispatch_id")?;
    one(&core().db.messages().lock(),
        "SELECT CASE kind WHEN 1 THEN blob WHEN 2 THEN thumb ELSE NULL END
         FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), did.as_slice()), |r| r.get::<_, Option<Vec<u8>>>(0))
        .map(Option::flatten).map_err(|e| anyhow::Error::from(e).into())
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
    remove(core(), targets).map_err(Into::into)
}

fn remove(c: &Core, targets: Vec<StorageTarget>) -> anyhow::Result<u32> {
    use crate::db::messages::MessageRow;
    let keys = targets.iter()
        .map(|t| {
            let conv = fixed::<16>(&t.conversation_id, "conversation id")?;
            Ok((conv, fixed::<16>(&t.dispatch_id, "dispatch_id")?))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut db = c.db.messages().lock();
    let tx = db.transaction()?;
    let mut removed = Vec::new();
    let mut orphans = Vec::new();
    for (conv, did) in keys {
        let row = one(&tx,
            "SELECT m.* FROM messages m JOIN message_media mm
             ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
             WHERE m.conversation_id = ?1 AND m.dispatch_id = ?2
             AND m.deleted = 0 AND NOT (m.outgoing = 1 AND m.status IN (0, 2))",
            (conv.as_slice(), did.as_slice()), MessageRow::from_row)?;
        let Some(row) = row else { continue; };
        let fid: Option<Vec<u8>> = tx.query_row(
            "SELECT file_id FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), did.as_slice()), |r| r.get(0))?;
        if let Some(fid) = fid.and_then(|f| <[u8; 32]>::try_from(f).ok()) {
            if crate::staging::holds(&fid) || store::partial_get_tx(&c.db.transfers().lock(), &fid)
                .is_some_and(|p| !p.is_complete()) { continue; }
        }
        tx.execute("DELETE FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), did.as_slice()))?;
        if let Some(fid) = crate::data::media::drop_row_tx(&tx, &conv, &did)? { orphans.push(fid); }
        removed.push((conv, row));
    }
    tx.commit()?;
    crate::data::media::unlink_orphaned(&c.db, &db, &orphans);
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
    use crate::data::media;
    use crate::data::media::MediaRow;
    use crate::data::message::Message;
    use crate::test_support::ScopedCore;

    /// Cleanup checks the whole request first, spares an active transfer and a pending send, keeps
    /// a file another chat still shows, and takes the caption with its media.
    #[tokio::test]
    async fn storage_cleanup_preserves_shared_files_and_pending_messages() {
        let scope = ScopedCore::new();
        let c = scope.core;
        let (conv, other, sender, fid) = ([181; 16], [182; 16], [183; 32], [184; 32]);
        let file = format!("{}/shared.bin", c.db.files_dir("transfers"));
        std::fs::write(&file, [42; 4096]).unwrap();
        let transfers = || c.db.transfers().lock();
        store::retention_put_tx(&transfers(), &fid, &file, 4096, 1024, &[], u64::MAX / 2).unwrap();
        let row = MediaRow {
            kind:        media::KIND_ATTACHMENT,
            group_id:    None,
            mime:        "application/octet-stream".into(),
            name:        "shared.bin".into(),
            size:        4096,
            width:       0,
            height:      0,
            duration_ms: 0,
            blob:        None,
            thumb:       Some(vec![1; 8]),
            file_id:     Some(fid.to_vec()),
            sticker:     None,
        };
        let save = |conv: &[u8; 16], did: u8, caption: &str, row: &MediaRow| {
            media::save_incoming_with_media(conv, &sender, &[did; 16], caption, 10, None, row)
                .unwrap()
        };
        save(&conv, 1, "caption", &row);
        save(&other, 2, "other caption", &row);
        let photo = MediaRow {
            kind: media::KIND_IMAGE,
            blob: Some(vec![9; 1024]),
            thumb: None,
            file_id: None,
            size: 1024,
            ..row.clone()
        };
        save(&conv, 3, "photo", &photo);
        let pending = media::save_outgoing_with_media(&conv, "sending", None, &photo).unwrap();
        let pending = pending.inner.dispatch_id.unwrap();

        let shown = inventory(c).unwrap();
        let bytes = |did| shown.iter().find(|r| r.dispatch_id == [did; 16]).unwrap().local_bytes;
        assert_eq!(shown.iter().filter(|r| r.conversation_id == conv).count(), 2);
        assert_eq!((bytes(1), bytes(3)), (4104, 1024));
        assert_eq!(storage_preview(conv.to_vec(), vec![3; 16]).unwrap().unwrap().len(), 1024);
        assert!(storage_preview(conv.to_vec(), vec![99; 16]).unwrap().is_none());

        let remove = |targets: &[([u8; 16], Vec<u8>)]| {
            let targets = targets.iter().map(|(conversation, dispatch_id)| StorageTarget {
                conversation_id: conversation.to_vec(),
                dispatch_id:     dispatch_id.clone(),
            });
            remove(c, targets.collect())
        };
        assert!(remove(&[(conv, vec![1; 16]), (conv, vec![0])]).is_err());
        assert!(media::get(&conv, &[1; 16]).unwrap().is_some());
        // The transfer started after the inventory was shown.
        let lease = store::receiver_lease(&c.db, fid);
        let mut partial = store::Partial {
            file_id:    fid,
            source_ipk: sender,
            total:      4096,
            chunk_size: 1024,
            manifest:   None,
            have:       1,
            state:      store::ACTIVE,
            path:       file.clone(),
            updated_at: 12,
        };
        store::partial_put_live_tx(&transfers(), &partial, &lease).unwrap();
        assert_eq!(remove(&[(conv, vec![1; 16])]).unwrap(), 0);
        assert!(!inventory(c).unwrap().iter().any(|r| r.dispatch_id == [1; 16]));
        (partial.state, partial.have) = (store::DONE, 4);
        store::partial_put_live_tx(&transfers(), &partial, &lease).unwrap();

        assert_eq!(remove(&[(conv, vec![1; 16]), (conv, pending.clone())]).unwrap(), 1);
        assert!(std::path::Path::new(&file).exists(), "another chat still shows this file");
        assert!(Message::get_by_dispatch(&conv, &pending.try_into().unwrap()).is_some());
        assert_eq!(remove(&[(other, vec![2; 16]), (conv, vec![3; 16])]).unwrap(), 2);
        assert!(!std::path::Path::new(&file).exists(), "the last reference releases the bytes");
        assert!(media::get(&conv, &[3; 16]).unwrap().is_none());
        assert!(Message::get_by_dispatch(&conv, &[3; 16]).is_none(), "the caption goes too");
        assert_eq!(remove(&[(conv, vec![3; 16])]).unwrap(), 0, "a stale selection");
    }
}
