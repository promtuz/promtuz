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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MediaRow {
    pub kind: u8,
    pub group_id: Option<Vec<u8>>,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Playback duration for voice and video.
    pub duration_ms: u32,
    pub blob: Option<Vec<u8>>,
    /// A blurred picture for an attachment, the loudness waveform for a voice note.
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    /// Serialized `StickerRef` for sending and downloading. Image bytes live in the cache.
    pub sticker: Option<Vec<u8>>,
    pub hd: Option<common::proto::mls_wire::ImageRendition>,
}

const PRIVATE_MEDIA_MIME: &str = "application/x-promtuz-private-media";

impl MediaRow {
    pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        if row.get::<_, String>("mime")? == PRIVATE_MEDIA_MIME {
            let conv: [u8; 16] = row.get("conversation_id")?;
            let bytes: Vec<u8> = row.get("blob")?;
            let decode = || -> anyhow::Result<Self> {
                let plain = crate::privacy::open_bytes(&conv, &bytes)?;
                Ok(postcard::from_bytes(&plain)?)
            };
            return decode().map_err(|_| rusqlite::Error::InvalidQuery);
        }
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
            // A private partial can also be named by this hash; its code-bound copy has its own owner.
            let private:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM private_file_refs WHERE file_id=?1)",[fid],|r|r.get(0)).unwrap_or(true);
            if !private {crate::transfer::store::forget_file(c, fid)}
        }
    }
    if let Err(error)=crate::private_files::cleanup_plaintext_tx(conn) {log::warn!("PRIVATE: media cleanup remains pending: {error}");}
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
    file_ids.extend(all(conn,"SELECT file_id FROM private_file_refs WHERE conversation_id=?1 AND dispatch_id=?2",(conv.as_slice(),dispatch_id),|r|r.get(0))?);
    Ok(file_ids)
}

pub fn save(conv: &[u8; 16], dispatch_id: &[u8; 16], r: &MediaRow) -> Result<()> {
    let db = core().db.messages().lock();
    save_tx(&db, conv, dispatch_id, r)
}

pub fn save_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], dispatch_id: &[u8; 16], r: &MediaRow,
) -> Result<()> {
    // Keep the complete media descriptor inside one authenticated envelope. File offers remain
    // in that envelope. File data keys are separately wrapped for the same private chat.
    let private=crate::privacy::is_private_tx(conn,conv)?;
    let private_files=if private {row_file_ids(r)} else {Vec::new()};
    let sealed;
    let r = if private {
        for file in row_file_ids(r) { crate::private_files::protect_tx(conn, conv, &file)?; }
        sealed = MediaRow {
            kind: r.kind, group_id: None, mime: PRIVATE_MEDIA_MIME.into(), name: String::new(),
            size: 0, width: 0, height: 0, duration_ms: 0,
            blob: Some(crate::privacy::seal_bytes_tx(conn, conv, &postcard::to_allocvec(r)?)?),
            thumb: None, file_id: None, sticker: None, hd: None,
        };
        &sealed
    } else { r };
    conn.execute(
        "INSERT OR REPLACE INTO message_media
         (conversation_id,dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id,duration_ms,sticker)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), r.kind, r.group_id,
            r.mime, r.name, r.size, r.width, r.height, r.blob, r.thumb, r.file_id, r.duration_ms,
            r.sticker],
    )?;
    for file in private_files {
        conn.execute("INSERT OR IGNORE INTO private_file_refs(conversation_id,dispatch_id,file_id) VALUES(?1,?2,?3)",(conv.as_slice(),dispatch_id.as_slice(),file.as_slice()))?;
    }
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

/// Publishes encrypted immutable file copies before replacing the ordinary media descriptors.
/// A failed conversion leaves the ordinary files and rows intact; unpublished ciphertext has no
/// plaintext key on disk and is collected by the storage sweep.
pub(crate) fn privatize_tx(conn: &rusqlite::Connection, conv: &[u8;16]) -> Result<()> {
    let rows = all(conn, "SELECT * FROM message_media WHERE conversation_id=?1", [conv],
        |r| Ok((r.get::<_,[u8;16]>("dispatch_id")?, MediaRow::from_row(r)?)))?;
    for (did, mut row) in rows {
        row.hd = one(conn, "SELECT mime,width,height,size,file_id FROM message_media_renditions WHERE conversation_id=?1 AND dispatch_id=?2 AND rendition=1",
            (conv.as_slice(),did.as_slice()), |r| Ok(common::proto::mls_wire::ImageRendition {
                mime:r.get(0)?,width:r.get(1)?,height:r.get(2)?,size:r.get(3)?,file_id:r.get(4)? }))?;
        save_tx(conn, conv, &did, &row)?;
    }
    let preparing:Vec<([u8;16],Vec<u8>)>=all(conn,"SELECT dispatch_id,details FROM message_media_preparations WHERE conversation_id=?1",[conv],|r|Ok((r.get(0)?,r.get(1)?)))?;
    for (did,details) in preparing {
        conn.execute("UPDATE message_media_preparations SET details=?3 WHERE conversation_id=?1 AND dispatch_id=?2",
            (conv.as_slice(),did.as_slice(),crate::privacy::seal_bytes_tx(conn,conv,&details)?))?;
    }
    Ok(())
}

/// Media decoded while its chat is still private, for writing back once it is not.
pub(crate) struct Snapshot { rows: Vec<([u8;16], MediaRow)>, preparing: Vec<([u8;16], Vec<u8>)> }
pub(crate) fn snapshot_tx(conn: &rusqlite::Connection, conv: &[u8;16]) -> Result<Snapshot> {
    let rows = all(conn, "SELECT * FROM message_media WHERE conversation_id=?1", [conv],
        |r| Ok((r.get::<_,[u8;16]>("dispatch_id")?, MediaRow::from_row(r)?)))?;
    let preparing = all(conn,"SELECT dispatch_id,details FROM message_media_preparations WHERE conversation_id=?1",[conv],
        |r|Ok((r.get::<_,[u8;16]>(0)?,r.get::<_,Vec<u8>>(1)?)))?
        .into_iter().map(|(did,details)| Ok((did, crate::privacy::open_if_sealed(conv,&details)?))).collect::<Result<_>>()?;
    Ok(Snapshot { rows, preparing })
}
pub(crate) fn restore_tx(conn: &rusqlite::Connection, conv: &[u8;16], snapshot: Snapshot) -> Result<()> {
    for (did, row) in &snapshot.rows { save_tx(conn, conv, did, row)?; }
    for (did, details) in snapshot.preparing {
        conn.execute("UPDATE message_media_preparations SET details=?3 WHERE conversation_id=?1 AND dispatch_id=?2",
            (conv.as_slice(),did.as_slice(),details))?;
    }
    Ok(())
}

pub(crate) fn row_file_ids(row:&MediaRow)->Vec<[u8;32]> {
    row.file_id.as_deref().and_then(|id|id.try_into().ok()).into_iter().chain(row.hd.as_ref().map(|hd|hd.file_id)).collect()
}

/// Private file references are read only from chat ciphertext and only for a live session.
pub(crate) fn private_file_rows_tx(conn:&rusqlite::Connection,file:&[u8;32])->Result<Vec<([u8;16],[u8;16],MediaRow)>> {
    let sessions=core().private_sessions.read().keys().copied().collect::<Vec<_>>();
    let mut found=Vec::new();
    for conv in sessions {
        let rows=all(conn,"SELECT * FROM message_media WHERE conversation_id=?1",[conv],|r|Ok((r.get::<_,[u8;16]>("dispatch_id")?,MediaRow::from_row(r)?)))?;
        for (did,row) in rows { if row_file_ids(&row).contains(file) {found.push((conv,did,row));} }
    }
    Ok(found)
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
        WHERE m.outgoing = 0 AND m.deleted = 0 AND NOT EXISTS(SELECT 1 FROM private_conversations pc WHERE pc.conversation_id=p.conversation_id)", [], |row| Ok(PreparationBackup {
            conversation_id: row.get(0)?, author: row.get(1)?, details: row.get(2)?,
        }))
}

pub fn restore_preparations_tx(conn: &rusqlite::Connection, rows: &[PreparationBackup]) -> Result<()> {
    for row in rows {
        if crate::privacy::is_private_tx(conn, &row.conversation_id)? { continue; }
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
    save_outgoing_full(conv, caption, reply_to, Some(r), None)
}

pub fn save_outgoing_full(
    conv: &[u8; 16], caption: &str, reply_to: Option<[u8; 16]>, r: Option<&MediaRow>,
    forwarded: Option<&str>,
) -> Result<crate::data::message::Message> {
    crate::privacy::require_access(conv)?;
    let me = crate::data::identity::Identity::local_ipk();
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let msg = crate::data::message::Message::save_outgoing_tx(&tx, *conv, caption, reply_to, me)?;
    let did: [u8; 16] = msg.inner.dispatch_id.as_slice().try_into().expect("dispatch_id is 16 bytes");
    if let Some(r) = r {
        save_tx(&tx, conv, &did, r)?;
        if r.kind == KIND_ATTACHMENT {
            tx.execute("INSERT INTO attachment_sharing_intents (message_id) VALUES (?1)",
                [msg.inner.id.to_string()])?;
        }
    }
    if let Some(from) = forwarded {
        crate::data::message::Message::set_forwarded_tx(&tx, conv, &did, from)?;
    }
    tx.commit()?;
    let Some(r) = r else { return Ok(msg) };
    if crate::privacy::is_private_tx(&db,conv)? { crate::private_files::cleanup_plaintext_tx(&db)?; }
    else { for file in row_file_ids(r) { crate::private_files::publish_ordinary(&file)?; } }
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
        rusqlite::params![crate::privacy::seal_tx(&tx, conv, content)?, conv.as_slice(), dispatch_id.as_slice(), own, author.map(|a| a.as_slice())],
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
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    let mut row = one(&tx, "SELECT * FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
        (conv.as_slice(), dispatch_id.as_slice()), MediaRow::from_row)?;
    if let Some(row) = &mut row {
        row.blob = Some(blob.to_vec()); row.size = blob.len() as u64;
        row.width = width; row.height = height;
        save_tx(&tx, conv, dispatch_id, row)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn set_file_id(conv: &[u8; 16], dispatch_id: &[u8; 16], file_id: &[u8; 32]) -> Result<()> {
    let mut db=core().db.messages().lock();
    let tx=db.transaction()?;
    if let Some(mut row)=one(&tx,"SELECT * FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",(conv.as_slice(),dispatch_id.as_slice()),MediaRow::from_row)? {
        row.file_id=Some(file_id.to_vec()); save_tx(&tx,conv,dispatch_id,&row)?;
    }
    tx.commit()?;
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
        if media.hd.is_some() { return Ok(Some(media.clone())) }
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
    let ordinary = one(
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
    )?;
    if ordinary.is_some() { return Ok(ordinary) }
    for (conv,did,row) in private_file_rows_tx(conn,file_id)? {
        let sender:Option<[u8;32]>=one(conn,"SELECT sender_ipk FROM messages WHERE conversation_id=?1 AND dispatch_id=?2 AND outgoing=0 AND deleted=0",(conv.as_slice(),did.as_slice()),|r|r.get(0))?.flatten();
        if let Some(sender)=sender { return Ok(Some((sender,if row.file_id.as_deref()==Some(file_id.as_slice()) {row.size}else{row.hd.map(|hd|hd.size).unwrap_or(0)}))) }
    }
    Ok(None)
}

pub fn groups_for(conv: &[u8; 16]) -> Result<std::collections::HashMap<Vec<u8>, Vec<u8>>> {
    let conn = core().db.messages().lock();
    let rows = all(&conn, "SELECT * FROM message_media WHERE conversation_id=?1", [conv],
        |row| Ok((row.get::<_,Vec<u8>>("dispatch_id")?, MediaRow::from_row(row)?)))?;
    Ok(rows.into_iter().filter_map(|(did, row)| row.group_id.map(|group| (did, group))).collect())
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
        "SELECT conversation_id, dispatch_id, sticker FROM message_media WHERE sticker IS NOT NULL AND NOT EXISTS(SELECT 1 FROM private_conversations pc WHERE pc.conversation_id=message_media.conversation_id)",
        [],
        StickerRefBackup::from_row,
    )
}

pub fn import_sticker_refs_tx(
    conn: &rusqlite::Connection, rows: &[StickerRefBackup],
) -> Result<usize> {
    let mut n = 0usize;
    for r in rows {
        if crate::privacy::is_private_tx(conn, &r.conversation_id)? { continue; }
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
                      WHERE NOT EXISTS(SELECT 1 FROM private_conversations pc WHERE pc.conversation_id=mm.conversation_id)
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
        if crate::privacy::is_private_tx(conn, &r.conversation_id)? { continue; }
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

    #[tokio::test]
    async fn private_inline_media_is_encrypted_on_disk_and_receives_while_locked() {
        let scope = crate::test_support::ScopedCore::new();
        let conv = [91; 16]; let did = [92; 16];
        scope.core.db.messages().lock().execute("INSERT INTO conversations(id) VALUES(?1)", [conv]).unwrap();
        let row = MediaRow {
            kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(), name: "secret-name".into(),
            size: 12, width: 16, height: 9, duration_ms: 0, blob: Some(b"private-photo".to_vec()),
            thumb: Some(b"private-preview".to_vec()), file_id: None, sticker: None, hd: None,
        };
        save(&conv, &did, &row).unwrap();
        crate::privacy::hide(&conv, "media-secret").unwrap();
        let (mime, name, blob, thumb): (String, String, Vec<u8>, Option<Vec<u8>>) = scope.core.db.messages().lock()
            .query_row("SELECT mime,name,blob,thumb FROM message_media", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_ne!(mime, row.mime); assert!(name.is_empty()); assert!(thumb.is_none());
        assert!(!blob.windows(13).any(|w| w == b"private-photo"));
        assert!(get(&conv, &did).is_err());
        // New incoming media can be sealed with only the public key retained.
        save(&conv, &[93;16], &row).unwrap();
        assert_eq!(crate::privacy::unlock("media-secret").unwrap(), Some(conv));
        assert_eq!(get(&conv, &did).unwrap(), Some(row.clone()));
        assert_eq!(get(&conv, &[93;16]).unwrap(), Some(row));
        // Identity recovery never provides a second way to recover private content.
        assert!(dump_all_tx(&scope.core.db.messages().lock()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn private_incoming_file_offer_never_exposes_a_plaintext_transfer_path() {
        let scope = crate::test_support::ScopedCore::new();
        let conv = [97;16]; let did = [98;16];
        scope.core.db.messages().lock().execute("INSERT INTO conversations(id) VALUES(?1)", [conv]).unwrap();
        crate::privacy::hide(&conv, "offer-secret").unwrap();
        let row = MediaRow {
            kind: KIND_ATTACHMENT, group_id: None, mime: "application/pdf".into(), name: "private.pdf".into(),
            size: 9, width: 0, height: 0, duration_ms: 0, blob: None,
            thumb: None, file_id: Some(vec![99;32]), sticker: None, hd: None,
        };
        save(&conv, &did, &row).unwrap();
        assert_eq!(file_ids_tx(&scope.core.db.messages().lock(), &conv, &did).unwrap().as_slice(), &[[99;32]]);
        assert_eq!(crate::privacy::unlock("offer-secret").unwrap(), Some(conv));
        let visible = crate::api::media::get_message_media(conv.to_vec(), did.to_vec()).unwrap().unwrap();
        assert_eq!(visible.name, "private.pdf");
        assert_eq!(visible.file_id, Some(vec![99;32])); assert!(visible.local_path.is_none());
    }

    #[tokio::test]
    async fn private_conversion_preserves_shared_authoritative_bytes_and_revokes_open_readers() {
        use std::io::Read;
        let scope = crate::test_support::ScopedCore::new();
        let (conv, ordinary, did) = ([94;16], [90;16], [95;16]);
        for id in [conv, ordinary] { scope.core.db.messages().lock().execute("INSERT INTO conversations(id) VALUES(?1)", [id]).unwrap(); }
        let source = format!("{}/source", scope.core.db.files_dir("picked"));
        let bytes = b"authoritative attachment: preserve every byte";
        std::fs::write(&source, bytes).unwrap();
        let (file, size) = crate::transfer::prepare_send(&source, 60, |_| {}).unwrap();
        crate::private_files::publish_ordinary(&file).unwrap();
        let old = crate::transfer::store::retention_get(&file).unwrap().path;
        let row = MediaRow { kind: KIND_ATTACHMENT, group_id: Some(vec![7;16]), mime: "application/pdf".into(),
            name: "keep.pdf".into(), size, width: 0, height: 0, duration_ms: 0, blob: None,
            thumb: None, file_id: Some(file.to_vec()), sticker: None, hd: None };
        save(&conv, &did, &row).unwrap(); save(&ordinary, &did, &row).unwrap();
        crate::privacy::hide(&conv, "file-secret").unwrap();
        assert_eq!(std::fs::read(&old).unwrap(), bytes, "the ordinary shared owner retains its source");
        assert!(get(&conv, &did).is_err());
        crate::privacy::unlock("file-secret").unwrap();
        assert_eq!(get(&conv, &did).unwrap(), Some(row));
        assert_eq!(groups_for(&conv).unwrap().get(did.as_slice()), Some(&vec![7;16]));
        let path = crate::private_files::retention_tx(&scope.core.db.messages().lock(), &conv, &file).unwrap().unwrap().path;
        assert_ne!(path, old); assert!(!std::fs::read(&path).unwrap().windows(bytes.len()).any(|part| part == bytes));
        let mut opened = crate::private_files::StorageFile::open(&path, false).unwrap();
        let mut decoded = Vec::new(); opened.read_to_end(&mut decoded).unwrap(); assert_eq!(decoded, bytes);
        let host = crate::api::media::open_private_media(conv.to_vec(), file.to_vec()).unwrap();
        assert_eq!(host.read(0, 1024).unwrap(), bytes);
        let conn = scope.core.db.messages().lock();
        let removed = drop_row_tx(&conn, &ordinary, &did).unwrap(); unlink_orphaned(scope.core, &conn, removed);
        drop(conn);
        assert!(!std::path::Path::new(&old).exists()); assert!(std::path::Path::new(&path).exists());
        crate::api::privacy::lock_private_conversations();
        assert!(opened.read(&mut [0;1]).is_err()); assert!(host.read(0, 1).is_err());
        crate::privacy::unlock("file-secret").unwrap();
        assert!(host.read(0, 1).is_err(), "an old capability stays revoked after reopening the chat");
        assert_eq!(crate::api::media::open_private_media(conv.to_vec(), file.to_vec()).unwrap().read(0, 1024).unwrap(), bytes);
        let conn = scope.core.db.messages().lock();
        let removed = drop_row_tx(&conn, &conv, &did).unwrap(); unlink_orphaned(scope.core, &conn, removed);
        assert!(!std::path::Path::new(&path).exists(), "the last private owner releases its own ciphertext");
    }

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
