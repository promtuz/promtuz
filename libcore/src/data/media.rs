//! Media rows keyed by (conversation_id, dispatch_id); the caption lives in `messages.content`.
use anyhow::Result;
use rusqlite::OptionalExtension;
use crate::db::all;
use crate::db::from_row;
use crate::db::one;
use crate::state::core;

pub const KIND_IMAGE: u8 = 1;
pub const KIND_ATTACHMENT: u8 = 2;
pub const KIND_VOICE: u8 = 3;
pub const KIND_STICKER: u8 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaRow {
    pub kind: u8,
    pub group_id: Option<Vec<u8>>,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Voice only.
    pub duration_ms: u32,
    pub blob: Option<Vec<u8>>,
    /// A blurred picture for an attachment, the loudness waveform for a voice note.
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    /// Serialized `StickerRef` for sending and downloading. Image bytes live in the cache.
    pub sticker: Option<Vec<u8>>,
}

from_row!(MediaRow {
    kind, group_id, mime, name, size, width, height, duration_ms, blob, thumb, file_id, sticker
});

/// Lock order: the messages lock (held), then the transfers or the staging lock; nothing takes
/// the messages lock while holding either. A file stays while a media row or the composer names it.
pub(crate) fn unlink_orphaned(
    db: &crate::db::Stores, conn: &rusqlite::Connection, file_ids: &[[u8; 32]],
) {
    for fid in file_ids {
        if crate::staging::holds(fid) {
            continue;
        }
        let sql = "SELECT 1 FROM message_media WHERE file_id = ?1 LIMIT 1";
        match conn.query_row(sql, [fid.as_slice()], |_| Ok(())) {
            // Nothing names it any more. Only this answer frees the bytes.
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                crate::transfer::store::forget_file(db, fid)
            },
            // A row still names it, or the read failed: a failed check is no permission to delete.
            _ => {},
        }
    }
}

/// Returns the `file_id` for the caller to [`unlink_orphaned`] after its write commits.
pub(crate) fn drop_row_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], dispatch_id: &[u8],
) -> Result<Option<[u8; 32]>> {
    let fid: Option<Vec<u8>> = conn
        .query_row(
            "SELECT file_id FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), dispatch_id),
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    conn.execute(
        "DELETE FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), dispatch_id),
    )?;
    Ok(fid.and_then(|f| f.try_into().ok()))
}

pub fn save(conv: &[u8; 16], dispatch_id: &[u8; 16], r: &MediaRow) -> Result<()> {
    let db = core().db.messages().lock();
    save_tx(&db, conv, dispatch_id, r)
}

pub fn save_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], dispatch_id: &[u8; 16], r: &MediaRow,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO message_media
         (conversation_id,dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id,duration_ms,sticker)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), r.kind, r.group_id,
            r.mime, r.name, r.size, r.width, r.height, r.blob, r.thumb, r.file_id, r.duration_ms,
            r.sticker],
    )?;
    Ok(())
}

/// Caption and media commit together: the ratchet is spent by now, so a partial could never heal.
pub fn save_incoming_with_media(
    conv: &[u8; 16], sender: &[u8; 32], dispatch_id: &[u8; 16], caption: &str, timestamp: u64,
    reply_to: Option<[u8; 16]>, r: &MediaRow,
) -> Result<Option<crate::data::message::Message>> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let saved = crate::data::message::Message::save_incoming_tx(
        &tx, *conv, *sender, dispatch_id, caption, timestamp, reply_to,
    )?;
    if saved.is_some() {
        save_tx(&tx, conv, dispatch_id, r)?;
    }
    tx.commit()?;
    Ok(saved)
}

pub fn save_outgoing_with_media(
    conv: &[u8; 16], caption: &str, reply_to: Option<[u8; 16]>, r: &MediaRow,
) -> Result<crate::data::message::Message> {
    let me = crate::data::identity::Identity::local_ipk();
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let msg = crate::data::message::Message::save_outgoing_tx(&tx, *conv, caption, reply_to, me)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .expect("save_outgoing mints a dispatch_id")
        .try_into()
        .expect("dispatch_id is 16 bytes");
    save_tx(&tx, conv, &did, r)?;
    if r.kind == KIND_ATTACHMENT {
        tx.execute("INSERT INTO attachment_sharing_intents (message_id) VALUES (?1)",
            [msg.inner.id.to_string()])?;
    }
    tx.commit()?;
    Ok(msg)
}

/// Same authorship guard as [`crate::data::message::Message::apply_edit`].
pub fn apply_revise(
    conv: &[u8; 16], dispatch_id: &[u8; 16], content: &str, media: Option<&MediaRow>, own: bool,
    author: Option<&[u8; 32]>,
) -> Result<Option<crate::db::messages::MessageRow>> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let n = tx.execute(
        "UPDATE messages SET content = ?1, edited = 1 \
         WHERE conversation_id = ?2 AND dispatch_id = ?3 AND outgoing = ?4 AND deleted = 0 \
           AND (?5 IS NULL OR sender_ipk = ?5)",
        rusqlite::params![content, conv.as_slice(), dispatch_id.as_slice(), own, author.map(|a| a.as_slice())],
    )?;
    if n == 0 {
        return Ok(None);
    }
    // The old row goes either way; its file survives only if the new body names it.
    let old = drop_row_tx(&tx, conv, dispatch_id)?;
    if let Some(r) = media {
        save_tx(&tx, conv, dispatch_id, r)?;
    }
    let row = tx.query_row(
        "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
        crate::db::messages::MessageRow::from_row,
    )?;
    tx.commit()?;
    unlink_orphaned(&core().db, &db, old.as_slice());
    Ok(Some(row))
}

pub fn set_blob(
    conv: &[u8; 16], dispatch_id: &[u8; 16], blob: &[u8], width: u32, height: u32,
) -> Result<()> {
    core().db.messages().lock().execute(
        "UPDATE message_media SET blob=?3, size=?4, width=?5, height=?6
         WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), blob, blob.len() as u64,
            width, height],
    )?;
    Ok(())
}

pub fn set_file_id(conv: &[u8; 16], dispatch_id: &[u8; 16], file_id: &[u8; 32]) -> Result<()> {
    core().db.messages().lock().execute(
        "UPDATE message_media SET file_id=?3 WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), file_id.as_slice()],
    )?;
    Ok(())
}

pub fn discard_outgoing(conv: &[u8; 16], dispatch_id: &[u8; 16]) -> Result<()> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
    )?;
    tx.execute(
        "DELETE FROM messages WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn kind(conv: &[u8; 16], dispatch_id: &[u8; 16]) -> Option<u8> {
    core().db.messages()
        .lock()
        .query_row(
            "SELECT kind FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
            rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
            |row| row.get(0),
        )
        .ok()
}

pub fn get(conv: &[u8; 16], dispatch_id: &[u8; 16]) -> Result<Option<MediaRow>> {
    Ok(one(
        &core().db.messages().lock(),
        "SELECT * FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
        MediaRow::from_row,
    )?)
}

/// The sender to pull from and the size they offered; the pull rejects a manifest that disagrees.
pub(crate) fn attachment_offer_tx(
    conn: &rusqlite::Connection, file_id: &[u8; 32],
) -> Result<Option<([u8; 32], u64)>> {
    Ok(one(
        conn,
        "SELECT m.sender_ipk, mm.size FROM message_media mm
           JOIN messages m ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE mm.file_id = ?1 AND m.outgoing = 0 AND m.sender_ipk IS NOT NULL LIMIT 1",
        [file_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub fn groups_for(conv: &[u8; 16]) -> Result<std::collections::HashMap<Vec<u8>, Vec<u8>>> {
    Ok(all(
        &core().db.messages().lock(),
        "SELECT dispatch_id, group_id FROM message_media WHERE conversation_id=?1 AND group_id IS NOT NULL",
        [conv.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?
    .into_iter()
    .collect())
}

pub fn for_conversation(conv: &[u8; 16], limit: u32) -> Result<Vec<([u8; 16], MediaRow)>> {
    Ok(all(
        &core().db.messages().lock(),
        "SELECT mm.* FROM message_media mm
         JOIN messages m ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE mm.conversation_id=?1 ORDER BY m.id DESC LIMIT ?2",
        rusqlite::params![conv.as_slice(), limit],
        |row| Ok((row.get("dispatch_id")?, MediaRow::from_row(row)?)),
    )?)
}

/// Kept apart from [`MediaBackupRow`], whose postcard layout every existing blob holds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StickerRefBackup {
    pub conversation_id: [u8; 16],
    pub dispatch_id:     [u8; 16],
    pub sticker:         Vec<u8>,
}

from_row!(StickerRefBackup { conversation_id, dispatch_id, sticker });

pub fn dump_sticker_refs_tx(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<Vec<StickerRefBackup>> {
    all(
        conn,
        "SELECT conversation_id, dispatch_id, sticker FROM message_media WHERE sticker IS NOT NULL",
        [],
        StickerRefBackup::from_row,
    )
}

pub fn import_sticker_refs_tx(
    conn: &rusqlite::Connection, rows: &[StickerRefBackup],
) -> Result<usize> {
    let mut n = 0usize;
    for r in rows {
        n += conn.execute(
            "UPDATE message_media SET sticker = ?3 \
             WHERE conversation_id = ?1 AND dispatch_id = ?2 AND sticker IS NULL",
            rusqlite::params![r.conversation_id.as_slice(), r.dispatch_id.as_slice(), r.sticker],
        )?;
    }
    Ok(n)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaBackupRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id: [u8; 16],
    #[serde(with = "serde_bytes")]
    pub dispatch_id: [u8; 16],
    pub kind: u8,
    pub group_id: Option<Vec<u8>>,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub blob: Option<Vec<u8>>,
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    pub duration_ms: u32,
}

/// Inline bytes a backup carries, newest first; older rows keep only their thumb and metadata, so
/// the blob fits the platform's backup quota.
const INLINE_MEDIA_BUDGET: usize = 16 << 20;

pub fn dump_all_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<MediaBackupRow>> {
    let (mut spent, mut dropped) = (0usize, 0usize);
    let rows = all(conn, "SELECT * FROM message_media ORDER BY rowid DESC", [], |r| {
        let blob = match r.get::<_, Option<Vec<u8>>>("blob")? {
            Some(b) if spent + b.len() <= INLINE_MEDIA_BUDGET => {
                spent += b.len();
                Some(b)
            },
            Some(_) => {
                dropped += 1;
                None
            },
            None => None,
        };
        Ok(MediaBackupRow {
            conversation_id: r.get("conversation_id")?,
            dispatch_id: r.get("dispatch_id")?,
            kind: r.get("kind")?,
            group_id: r.get("group_id")?,
            mime: r.get("mime")?,
            name: r.get("name")?,
            size: r.get("size")?,
            width: r.get("width")?,
            height: r.get("height")?,
            blob,
            thumb: r.get("thumb")?,
            file_id: r.get("file_id")?,
            duration_ms: r.get("duration_ms")?,
        })
    })?;
    if dropped > 0 {
        log::info!("BACKUP: {dropped} inline media blob(s) left out past the {INLINE_MEDIA_BUDGET} byte budget");
    }
    Ok(rows)
}

/// Oldest dispatch first, whatever order the blob lists them in, so rowids rise with age as the
/// export's inline budget expects.
pub fn import_rows_tx(conn: &rusqlite::Connection, rows: &[MediaBackupRow]) -> Result<usize> {
    let mut rows: Vec<_> = rows.iter().collect();
    rows.sort_by_key(|r| r.dispatch_id);
    let mut n = 0usize;
    for r in rows {
        n += conn.execute(
            "INSERT OR IGNORE INTO message_media \
             (conversation_id, dispatch_id, kind, group_id, mime, name, size, width, height, blob, thumb, file_id, duration_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                r.conversation_id.as_slice(),
                r.dispatch_id.as_slice(),
                r.kind,
                r.group_id.as_deref(),
                r.mime,
                r.name,
                r.size,
                r.width,
                r.height,
                r.blob.as_deref(),
                r.thumb.as_deref(),
                r.file_id.as_deref(),
                r.duration_ms,
            ],
        )?;
    }
    Ok(n)
}
