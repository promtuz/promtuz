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
pub const RENDITION_HD_IMAGE: u8 = 1;

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
    pub hd: Option<common::proto::mls_wire::ImageRendition>,
}

impl MediaRow {
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        Ok(Self {
            kind: row.get("kind")?, group_id: row.get("group_id")?, mime: row.get("mime")?,
            name: row.get("name")?, size: row.get("size")?, width: row.get("width")?,
            height: row.get("height")?, duration_ms: row.get("duration_ms")?,
            blob: row.get("blob")?, thumb: row.get("thumb")?, file_id: row.get("file_id")?,
            sticker: row.get("sticker")?, hd: None,
        })
    }
}

#[derive(Default)]
pub(crate) struct MediaFileIds(Vec<[u8; 32]>);

impl MediaFileIds {
    pub(crate) fn as_slice(&self) -> &[[u8; 32]] { &self.0 }

    fn push(&mut self, file_id: Option<Vec<u8>>) {
        if let Some(file_id) = file_id.and_then(|id| id.try_into().ok()) {
            self.0.push(file_id);
        }
    }

    pub(crate) fn extend(&mut self, file_ids: impl IntoIterator<Item = Vec<u8>>) {
        for file_id in file_ids {
            self.push(Some(file_id));
        }
    }
}

impl AsRef<[[u8; 32]]> for MediaFileIds {
    fn as_ref(&self) -> &[[u8; 32]] { &self.0 }
}

/// Lock order: the messages lock (held), then the transfers or the staging lock; nothing takes
/// the messages lock while holding either. A file stays while a media row or the composer names it.
pub(crate) fn unlink_orphaned(
    c: &crate::state::Core, conn: &rusqlite::Connection, file_ids: impl AsRef<[[u8; 32]]>,
) {
    for fid in file_ids.as_ref() {
        if crate::staging::holds(c, fid) {
            continue;
        }
        let sql = "SELECT 1 FROM message_media WHERE file_id = ?1
                   UNION ALL SELECT 1 FROM message_media_renditions WHERE file_id = ?1 LIMIT 1";
        // Only a missing row permits deletion; a failed read must retain the file.
        if let Err(rusqlite::Error::QueryReturnedNoRows) = conn.query_row(sql, [fid.as_slice()], |_| Ok(())) {
            crate::transfer::store::forget_file(c, fid)
        }
    }
}

/// Returns every transfer-store file the caller may unlink after its write commits.
pub(crate) fn drop_row_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], dispatch_id: &[u8],
) -> Result<MediaFileIds> {
    let file_ids = file_ids_tx(conn, conv, dispatch_id)?;
    conn.execute(
        "DELETE FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), dispatch_id),
    )?;
    Ok(file_ids)
}

pub(crate) fn file_ids_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], dispatch_id: &[u8],
) -> Result<MediaFileIds> {
    let mut file_ids = MediaFileIds::default();
    file_ids.push(conn
        .query_row(
            "SELECT file_id FROM message_media WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conv.as_slice(), dispatch_id),
            |r| r.get(0),
        )
        .optional()?
        .flatten());
    file_ids.extend(all(
        conn,
        "SELECT file_id FROM message_media_renditions WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), dispatch_id),
        |row| row.get(0),
    )?);
    Ok(file_ids)
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
    conn.execute(
        "DELETE FROM message_media_renditions WHERE conversation_id = ?1 AND dispatch_id = ?2",
        (conv.as_slice(), dispatch_id.as_slice()),
    )?;
    if let Some(hd) = &r.hd {
        conn.execute(
            "INSERT INTO message_media_renditions
             (conversation_id, dispatch_id, rendition, mime, width, height, size, file_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                conv.as_slice(), dispatch_id.as_slice(), RENDITION_HD_IMAGE, hd.mime, hd.width,
                hd.height, hd.size, hd.file_id.as_slice(),
            ],
        )?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PreparationBackup {
    conversation_id: [u8; 16],
    author: [u8; 32],
    details: Vec<u8>,
}

pub fn dump_preparations_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<PreparationBackup>> {
    all(conn, "SELECT p.conversation_id, m.sender_ipk, p.details
        FROM message_media_preparations p JOIN messages m
          ON m.conversation_id = p.conversation_id AND m.dispatch_id = p.dispatch_id
        WHERE m.outgoing = 0 AND m.deleted = 0", [], |row| Ok(PreparationBackup {
            conversation_id: row.get(0)?, author: row.get(1)?, details: row.get(2)?,
        }))
}

pub fn restore_preparations_tx(conn: &rusqlite::Connection, rows: &[PreparationBackup]) -> Result<()> {
    for row in rows {
        let pending: common::proto::media::PreparingMedia = postcard::from_bytes(&row.details)?;
        conn.execute("INSERT OR IGNORE INTO message_media_preparations (conversation_id, dispatch_id, details)
            SELECT ?1, ?2, ?3 FROM messages m JOIN message_media mm
              ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
            WHERE m.conversation_id = ?1 AND m.dispatch_id = ?2 AND m.sender_ipk = ?4
              AND m.outgoing = 0 AND m.deleted = 0 AND mm.kind = 2 AND mm.file_id IS NULL
              AND mm.mime = ?5 AND m.content = ?6",
            (row.conversation_id.as_slice(), pending.target.as_slice(), &row.details,
                row.author.as_slice(), &pending.mime, &pending.caption))?;
    }
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
    let did: [u8; 16] = msg.inner.dispatch_id.as_slice().try_into().expect("dispatch_id is 16 bytes");
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
    unlink_orphaned(core(), &db, old);
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
    let mut media = one(
        &core().db.messages().lock(),
        "SELECT * FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
        MediaRow::from_row,
    )?;
    if let Some(media) = &mut media {
        media.hd = one(
            &core().db.messages().lock(),
            "SELECT mime, width, height, size, file_id FROM message_media_renditions
             WHERE conversation_id = ?1 AND dispatch_id = ?2 AND rendition = ?3",
            rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), RENDITION_HD_IMAGE],
            |row| Ok(common::proto::mls_wire::ImageRendition {
                mime: row.get(0)?, width: row.get(1)?, height: row.get(2)?, size: row.get(3)?,
                file_id: row.get::<_, Vec<u8>>(4)?.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?,
            }),
        )?;
    }
    Ok(media)
}

/// The sender to pull from and the size they offered; the pull rejects a manifest that disagrees.
pub(crate) fn attachment_offer_tx(
    conn: &rusqlite::Connection, file_id: &[u8; 32],
) -> Result<Option<([u8; 32], u64)>> {
    Ok(one(
        conn,
        "SELECT m.sender_ipk, mm.size FROM message_media mm
           JOIN messages m ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE mm.file_id = ?1 AND m.outgoing = 0 AND m.sender_ipk IS NOT NULL
         UNION ALL
         SELECT m.sender_ipk, hd.size FROM message_media_renditions hd
           JOIN messages m ON m.conversation_id = hd.conversation_id AND m.dispatch_id = hd.dispatch_id
         WHERE hd.file_id = ?1 AND m.outgoing = 0 AND m.sender_ipk IS NOT NULL LIMIT 1",
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
    pub hd: Option<common::proto::mls_wire::ImageRendition>,
}

/// Inline bytes a backup carries, newest first; older rows keep only their thumb and metadata, so
/// the blob fits the platform's backup quota.
const INLINE_MEDIA_BUDGET: usize = 16 << 20;

pub fn dump_all_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<MediaBackupRow>> {
    let (mut spent, mut dropped) = (0usize, 0usize);
    let rows = all(conn, "SELECT mm.*, hd.mime AS hd_mime, hd.width AS hd_width,
                          hd.height AS hd_height, hd.size AS hd_size, hd.file_id AS hd_file_id
                       FROM message_media mm
                       LEFT JOIN message_media_renditions hd
                         ON hd.conversation_id = mm.conversation_id
                        AND hd.dispatch_id = mm.dispatch_id
                        AND hd.rendition = 1
                      ORDER BY mm.rowid DESC", [], |r| {
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
            hd: r.get::<_, Option<String>>("hd_mime")?.map(|mime| {
                Ok::<_, rusqlite::Error>(common::proto::mls_wire::ImageRendition {
                    mime,
                    width: r.get("hd_width")?,
                    height: r.get("hd_height")?,
                    size: r.get("hd_size")?,
                    file_id: r.get::<_, Vec<u8>>("hd_file_id")?
                        .try_into().map_err(|_| rusqlite::Error::InvalidQuery)?,
                })
            }).transpose()?,
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
        if let Some(hd) = &r.hd {
            n += conn.execute(
                "INSERT OR IGNORE INTO message_media_renditions
                 (conversation_id, dispatch_id, rendition, mime, width, height, size, file_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    r.conversation_id.as_slice(), r.dispatch_id.as_slice(), RENDITION_HD_IMAGE,
                    hd.mime, hd.width, hd.height, hd.size, hd.file_id.as_slice(),
                ],
            )?;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::data::open;

    #[test]
    fn hd_rendition_is_transactional_with_its_image_row() {
        let conn = open(crate::db::messages::migrate);
        let conv = [1; 16];
        let did = [2; 16];
        let hd = common::proto::mls_wire::ImageRendition {
            mime: "image/avif".into(), width: 2560, height: 1440, size: 1234, file_id: [3; 32],
        };
        let row = MediaRow {
            kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(), name: String::new(),
            size: 12, width: 16, height: 9, duration_ms: 0, blob: Some(vec![1; 12]), thumb: None,
            file_id: None, sticker: None, hd: Some(hd.clone()),
        };
        save_tx(&conn, &conv, &did, &row).unwrap();
        let stored: (String, u32, u32, u64, Vec<u8>) = conn.query_row(
            "SELECT mime, width, height, size, file_id FROM message_media_renditions",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).unwrap();
        assert_eq!(stored, (hd.mime, hd.width, hd.height, hd.size, hd.file_id.to_vec()));
        assert_eq!(drop_row_tx(&conn, &conv, &did).unwrap().as_slice(), [[3; 32]]);
        let count: u32 = conn.query_row("SELECT COUNT(*) FROM message_media_renditions", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
    }
}
