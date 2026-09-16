//! Per-message media metadata (Image / Voice inline bytes, Attachment thumb +
//! file_id), keyed by (conversation_id, dispatch_id). The caption itself lives
//! on messages.content.
use anyhow::Result;
use rusqlite::OptionalExtension;
use crate::db::messages::MESSAGES_DB;

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
    /// The small preview drawn before the real thing: a blurred picture for an
    /// attachment, the loudness waveform for a voice note.
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    /// Serialized `StickerRef` for sending and downloading. Image bytes live in the cache.
    pub sticker: Option<Vec<u8>>,
}

/// Have the transfer store forget attachments whose media rows are now
/// committed away — otherwise deleting a chat to be rid of a photo keeps the
/// photo. The store owns both the bytes and the rows that find them, so the
/// removal happens there rather than by reaching into its storage layout.
///
/// Re-checked against the whole of `message_media` first: the same content can
/// hang off a second row in another chat, and the rows are the source of truth.
/// The composer buffer counts as a holder too — a chip readied from the same
/// document as the message being deleted is about to need the file.
///
/// Runs with `MESSAGES_DB` held and takes `TRANSFERS_DB` (and, briefly, the
/// staging buffer's lock) inside it. That is the only direction any of them
/// are ever held in — every `TRANSFERS_DB` scope lives in `transfer::store`,
/// staging releases its lock before it reaches for this one, and none reaches
/// back for `MESSAGES_DB`. One that did would close the cycle and hang, as
/// would a commit hook that called into core rather than just waking the UI.
pub(crate) fn unlink_orphaned(conn: &rusqlite::Connection, file_ids: &[[u8; 32]]) {
    for fid in file_ids {
        if crate::staging::holds(fid) {
            continue;
        }
        let sql = "SELECT 1 FROM message_media WHERE file_id = ?1 LIMIT 1";
        match conn.query_row(sql, [fid.as_slice()], |_| Ok(())) {
            // Nothing names it any more. Only this answer frees the bytes.
            Err(rusqlite::Error::QueryReturnedNoRows) => crate::transfer::store::forget_file(fid),
            // A row still names it — or the read that decides just failed, and
            // a failure to consult the source of truth is not permission to
            // delete what another chat may still be showing. Keep the file.
            _ => {},
        }
    }
}

/// Drop one message's media row — inline bytes included — and return the
/// `file_id` it named, for the caller to [`unlink_orphaned`] once its own
/// write is in. Separate steps because the row is what [`unlink_orphaned`]
/// consults, so it must already be gone when that check runs.
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
    let db = MESSAGES_DB.lock();
    save_tx(&db, conv, dispatch_id, r)
}

/// Transaction-scoped [`save`]: writes the media row against a caller-supplied
/// connection so it can share one transaction with the caption insert.
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

/// Atomically persist an incoming media message: its caption row on `messages`
/// and its media row on `message_media`, in ONE transaction — either both land
/// or neither does. On a media-write failure the caption rolls back and the
/// error propagates, so the (un-acked) message redelivers whole rather than
/// becoming a permanent caption-only orphan (the MLS ratchet is spent by
/// receive time, so a partial can never self-heal). Returns the caption row,
/// or `None` when the dispatch_id was already stored (redelivery: a clean
/// no-op that still commits, so the caller acks and the relay GCs).
pub fn save_incoming_with_media(
    conv: &[u8; 16], sender: &[u8; 32], dispatch_id: &[u8; 16], caption: &str, timestamp: u64,
    reply_to: Option<[u8; 16]>, r: &MediaRow,
) -> Result<Option<crate::data::message::Message>> {
    let mut db = MESSAGES_DB.lock();
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

/// Atomically persist an outgoing media message: its caption row on `messages`
/// and its media row on `message_media`, in ONE transaction — the send-side
/// mirror of [`save_incoming_with_media`]. A media-write failure rolls the
/// caption back instead of committing a caption-only orphan with no picture and
/// no retry. The media row keys off the freshly-minted dispatch_id.
pub fn save_outgoing_with_media(
    conv: &[u8; 16], caption: &str, reply_to: Option<[u8; 16]>, r: &MediaRow,
) -> Result<crate::data::message::Message> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    let msg = crate::data::message::Message::save_outgoing_tx(&tx, *conv, caption, reply_to)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .expect("save_outgoing mints a dispatch_id")
        .try_into()
        .expect("dispatch_id is 16 bytes");
    save_tx(&tx, conv, &did, r)?;
    tx.commit()?;
    Ok(msg)
}

/// Swap a stored message's body in ONE transaction: its text/caption on
/// `messages` (flagging `edited`) and its media side-row on `message_media` —
/// replaced when the new body carries media, dropped when it doesn't, so a
/// revision never leaves a stale picture under fresh text. Same authorship
/// guard as [`crate::data::message::Message::apply_edit`]: `own = true` for our
/// own revision, `false` for an inbound peer one, plus the per-member `author`
/// check in a group, so nobody can revise another member's messages. `None`
/// when the target is missing, tombstoned, or authored by someone else.
pub fn apply_revise(
    conv: &[u8; 16], dispatch_id: &[u8; 16], content: &str, media: Option<&MediaRow>, own: bool,
    author: Option<&[u8; 32]>,
) -> Result<Option<crate::db::messages::MessageRow>> {
    let mut db = MESSAGES_DB.lock();
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
    // The old side-row goes either way; what it named is orphaned unless the
    // new body names the same file.
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
    unlink_orphaned(&db, old.as_slice());
    Ok(Some(row))
}

/// Fill an outgoing image's compressed bytes + final size/dims once encoding
/// finishes (the placeholder row was inserted with a null blob so the bubble
/// could show instantly).
pub fn set_blob(
    conv: &[u8; 16], dispatch_id: &[u8; 16], blob: &[u8], width: u32, height: u32,
) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "UPDATE message_media SET blob=?3, size=?4, width=?5, height=?6
         WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), blob, blob.len() as u64,
            width, height],
    )?;
    Ok(())
}

/// Fill an outgoing attachment's content-addressed file_id once the manifest
/// pass finishes (placeholder inserted with a null file_id).
pub fn set_file_id(conv: &[u8; 16], dispatch_id: &[u8; 16], file_id: &[u8; 32]) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "UPDATE message_media SET file_id=?3 WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice(), file_id.as_slice()],
    )?;
    Ok(())
}

/// Remove an outgoing media message wholesale — caption row + media side-row —
/// when the heavy prep (compress / manifest) fails before the send ever
/// started, so no dead placeholder bubble lingers. One transaction.
pub fn discard_outgoing(conv: &[u8; 16], dispatch_id: &[u8; 16]) -> Result<()> {
    let mut db = MESSAGES_DB.lock();
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

/// The media side-row for one message (by peer + dispatch_id), or `None` if
/// the message carries no media. Lets the send-retry path rebuild the original
/// media payload instead of downgrading it to bare text.
pub fn get(conv: &[u8; 16], dispatch_id: &[u8; 16]) -> Result<Option<MediaRow>> {
    let db = MESSAGES_DB.lock();
    db.query_row(
        "SELECT kind,group_id,mime,name,size,width,height,blob,thumb,file_id,duration_ms,sticker
         FROM message_media WHERE conversation_id=?1 AND dispatch_id=?2",
        rusqlite::params![conv.as_slice(), dispatch_id.as_slice()],
        |row| Ok(MediaRow {
            kind: row.get(0)?, group_id: row.get(1)?, mime: row.get(2)?, name: row.get(3)?,
            size: row.get(4)?, width: row.get(5)?, height: row.get(6)?,
            blob: row.get(7)?, thumb: row.get(8)?, file_id: row.get(9)?, duration_ms: row.get(10)?,
            sticker: row.get(11)?,
        }),
    )
    .optional()
    .map_err(Into::into)
}

/// The member to dial for an incoming attachment and the size they advertised
/// in the offer — the pull rejects a manifest whose `total_size` belies it.
/// Read off `messages.sender_ipk`, so in a group the file is pulled from
/// whoever actually sent it rather than from the conversation at large.
/// Restricted to the INCOMING row (`m.outgoing = 0`): if we both received and
/// re-sent the same content-addressed file, the outgoing row names our own
/// recipient (who serves `Gone`), not the sender we must pull from.
pub fn attachment_offer(file_id: &[u8; 32]) -> Result<Option<([u8; 32], u64)>> {
    let db = MESSAGES_DB.lock();
    db.query_row(
        "SELECT m.sender_ipk, mm.size FROM message_media mm
           JOIN messages m ON m.conversation_id = mm.conversation_id AND m.dispatch_id = mm.dispatch_id
         WHERE mm.file_id = ?1 AND m.outgoing = 0 AND m.sender_ipk IS NOT NULL LIMIT 1",
        [file_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

pub fn for_conversation(conv: &[u8; 16]) -> Result<Vec<([u8; 16], MediaRow)>> {
    let db = MESSAGES_DB.lock();
    let mut stmt = db.prepare(
        "SELECT dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id,duration_ms,sticker
         FROM message_media WHERE conversation_id=?1")?;
    let rows = stmt.query_map([conv.as_slice()], |row| {
        let did: Vec<u8> = row.get(0)?;
        let mut d = [0u8; 16]; d.copy_from_slice(&did);
        Ok((d, MediaRow {
            kind: row.get(1)?, group_id: row.get(2)?, mime: row.get(3)?, name: row.get(4)?,
            size: row.get(5)?, width: row.get(6)?, height: row.get(7)?,
            blob: row.get(8)?, thumb: row.get(9)?, file_id: row.get(10)?, duration_ms: row.get(11)?,
            sticker: row.get(12)?,
        }))
    })?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// A sticker row's reference, keyed the way the backup carries it. Lives
/// beside [`MediaBackupRow`] rather than in it: that row's postcard layout is
/// what every existing blob holds, so the reference travels as a separate
/// list appended after the payload.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StickerRefBackup {
    pub conversation_id: [u8; 16],
    pub dispatch_id:     [u8; 16],
    pub sticker:         Vec<u8>,
}

pub fn dump_sticker_refs() -> Vec<StickerRefBackup> {
    let conn = MESSAGES_DB.lock();
    let Ok(mut stmt) = conn.prepare(
        "SELECT conversation_id, dispatch_id, sticker FROM message_media WHERE sticker IS NOT NULL",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], |r| {
        let conv: Vec<u8> = r.get(0)?;
        let did: Vec<u8> = r.get(1)?;
        Ok(StickerRefBackup {
            conversation_id: conv.try_into().unwrap_or([0u8; 16]),
            dispatch_id:     did.try_into().unwrap_or([0u8; 16]),
            sticker:         r.get(2)?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// Put restored sticker rows' references back. Only rows still missing one
/// change, so a live reference is never overwritten by the snapshot's.
pub fn import_sticker_refs(rows: &[StickerRefBackup]) -> Result<usize> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    let mut n = 0usize;
    for r in rows {
        n += tx.execute(
            "UPDATE message_media SET sticker = ?3 \
             WHERE conversation_id = ?1 AND dispatch_id = ?2 AND sticker IS NULL",
            rusqlite::params![r.conversation_id.as_slice(), r.dispatch_id.as_slice(), r.sticker],
        )?;
    }
    tx.commit()?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    /// Stand-in for the member who authored an inbound test message.
    const SENDER: [u8; 32] = [0xEE; 32];

    use super::*;

    #[test]
    fn media_row_saves_and_reads_back() {
        // MESSAGES_DB is a process-global Lazy; point it at a scratch dir before
        // the first touch (mirrors delivery/mod.rs's OUTBOX_DB test pattern —
        // db() exits the process if PROMTUZ_DATA_DIR is unset).
        let dir = std::env::temp_dir().join("promtuz-media-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        // NB: uses the shared MESSAGES_DB; run with --test-threads=1 if the DB is process-global.
        let conv = [3u8; 16]; let did = [4u8; 16];
        let row = MediaRow { kind: KIND_IMAGE, group_id: Some(vec![1u8;16]),
            mime: "image/avif".into(), name: "".into(), size: 3, width: 4, height: 3,
            blob: Some(vec![9,9,9]), thumb: None, file_id: None , duration_ms: 0, sticker: None};
        save(&conv, &did, &row).unwrap();
        let got = for_conversation(&conv).unwrap();
        assert!(got.iter().any(|(d, r)| *d == did && r.blob == row.blob && r.kind == KIND_IMAGE));
    }

    /// Two-phase optimistic send: a placeholder saved with a null blob is
    /// filled in place by `set_blob` once the encode finishes.
    #[test]
    fn set_blob_fills_placeholder() {
        let dir = std::env::temp_dir().join("promtuz-media-setblob-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let conv = [0x21u8; 16];
        let did = [0x22u8; 16];
        let row = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: "".into(), size: 0, width: 4, height: 3,
            blob: None, thumb: None, file_id: None , duration_ms: 0, sticker: None};
        save(&conv, &did, &row).unwrap();
        assert!(get(&conv, &did).unwrap().unwrap().blob.is_none());

        set_blob(&conv, &did, &[7, 8, 9], 2, 2).unwrap();
        let got = get(&conv, &did).unwrap().unwrap();
        assert_eq!(got.blob, Some(vec![7, 8, 9]));
        assert_eq!(got.size, 3);
        assert_eq!((got.width, got.height), (2, 2));
    }

    /// "Deleted" has to mean the picture too: a tombstone that kept the media
    /// row would leave the bytes in the database behind an empty caption.
    #[test]
    fn tombstone_takes_the_media_row_with_it() {
        let dir = std::env::temp_dir().join("promtuz-media-tombstone-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let conv = [0x31u8; 16];
        let row = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: "".into(), size: 3, width: 4, height: 3,
            blob: Some(vec![9, 9, 9]), thumb: None, file_id: None , duration_ms: 0, sticker: None};
        let msg = save_outgoing_with_media(&conv, "", None, &row).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.as_deref().unwrap().try_into().unwrap();
        assert!(get(&conv, &did).unwrap().is_some());

        let gone = crate::data::message::Message::apply_delete(&conv, &did, true, None).unwrap();
        assert!(gone.deleted, "the caption row is tombstoned");
        assert!(get(&conv, &did).unwrap().is_none(), "and the media row is gone");
    }

    /// A failed prep must not leave a dead placeholder bubble: both the
    /// caption row and the media side-row go.
    #[test]
    fn discard_outgoing_removes_caption_and_media() {
        let dir = std::env::temp_dir().join("promtuz-media-discard-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let conv = [0x23u8; 16];
        let row = MediaRow { kind: KIND_ATTACHMENT, group_id: None,
            mime: "application/pdf".into(), name: "a.pdf".into(), size: 9,
            width: 0, height: 0, blob: None, thumb: None, file_id: None , duration_ms: 0, sticker: None};
        let msg = save_outgoing_with_media(&conv, "cap", None, &row).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();
        assert!(get(&conv, &did).unwrap().is_some());

        discard_outgoing(&conv, &did).unwrap();
        assert!(get(&conv, &did).unwrap().is_none(), "media side-row gone");
        assert!(
            crate::data::message::Message::get_by_dispatch(&conv, &did).is_none(),
            "caption row gone"
        );
    }

    /// The atomicity guarantee behind `save_incoming_with_media`: caption and
    /// media commit together, and a media-write failure inside the transaction
    /// rolls the caption back — no permanent caption-only orphan. Driven on an
    /// in-memory connection with the real tx-scoped helpers. (The real trigger
    /// is SQLITE_BUSY / disk-full, unforceable in a unit test; a NOT NULL
    /// violation stands in as the failing media write.)
    #[test]
    fn caption_and_media_are_atomic() {
        use crate::data::message::Message;
        fn count(conn: &rusqlite::Connection, sql: &str, k: &[u8]) -> i64 {
            conn.query_row(sql, [k], |r| r.get(0)).unwrap()
        }
        let mut conn = crate::db::messages::open_in_memory();
        let conv = [5u8; 16];

        // Happy path: both rows land in one committed transaction.
        let did = [6u8; 16];
        let media = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: String::new(), size: 3, width: 1, height: 1,
            blob: Some(vec![1, 2, 3]), thumb: None, file_id: None , duration_ms: 0, sticker: None};
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::save_incoming_tx(&tx, conv, SENDER, &did, "cap", 100, None).unwrap().is_some());
            save_tx(&tx, &conv, &did, &media).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did.as_slice()), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_media WHERE dispatch_id=?1", did.as_slice()), 1);

        // Rollback path: a failing media write undoes the caption row already
        // inserted in the same transaction.
        let did2 = [7u8; 16];
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::save_incoming_tx(&tx, conv, SENDER, &did2, "cap2", 100, None).unwrap().is_some());
            let bad = tx.execute(
                "INSERT INTO message_media (conversation_id,dispatch_id,kind,mime) VALUES (?1,?2,NULL,?3)",
                rusqlite::params![conv.as_slice(), did2.as_slice(), "image/avif"],
            );
            assert!(bad.is_err(), "NULL kind must violate NOT NULL");
            // tx dropped without commit → rollback
        }
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did2.as_slice()), 0,
            "media failure rolled back the caption — no orphan");
    }

    /// The send-side mirror of `caption_and_media_are_atomic`: an outgoing
    /// image persists its caption and its AVIF media row together, and a
    /// failing media write rolls the caption back — no caption-only orphan the
    /// send path can never repair. Driven on an in-memory connection with the
    /// real tx-scoped helpers (`save_outgoing_tx` + `save_tx`, the exact pair
    /// `save_outgoing_with_media` composes).
    #[test]
    fn outgoing_caption_and_media_are_atomic() {
        use crate::data::message::Message;
        fn count(conn: &rusqlite::Connection, sql: &str, k: &[u8]) -> i64 {
            conn.query_row(sql, [k], |r| r.get(0)).unwrap()
        }
        let mut conn = crate::db::messages::open_in_memory();
        let conv = [8u8; 16];
        let media = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: String::new(), size: 3, width: 4, height: 3,
            blob: Some(vec![1, 2, 3]), thumb: None, file_id: None , duration_ms: 0, sticker: None};

        // Happy path: caption + media land in one committed transaction.
        let did: [u8; 16] = {
            let tx = conn.transaction().unwrap();
            let msg = Message::save_outgoing_tx(&tx, conv, "cap", None).unwrap();
            let d: [u8; 16] = msg.inner.dispatch_id.unwrap().try_into().unwrap();
            save_tx(&tx, &conv, &d, &media).unwrap();
            tx.commit().unwrap();
            d
        };
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did.as_slice()), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_media WHERE dispatch_id=?1", did.as_slice()), 1);

        // Rollback path: a failing media write undoes the caption already
        // inserted in the same transaction.
        let did2: [u8; 16] = {
            let tx = conn.transaction().unwrap();
            let msg = Message::save_outgoing_tx(&tx, conv, "cap2", None).unwrap();
            let d: [u8; 16] = msg.inner.dispatch_id.unwrap().try_into().unwrap();
            let bad = tx.execute(
                "INSERT INTO message_media (conversation_id,dispatch_id,kind,mime) VALUES (?1,?2,NULL,?3)",
                rusqlite::params![conv.as_slice(), d.as_slice(), "image/avif"],
            );
            assert!(bad.is_err(), "NULL kind must violate NOT NULL");
            d // tx dropped without commit → rollback
        };
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did2.as_slice()), 0,
            "media failure rolled back the caption — no orphan");
    }
}

/// One media row with the key it hangs off, for the backup snapshot. `MediaRow`
/// itself is keyless because every live caller already knows the message it is
/// asking about; a blob has to carry the key with the value.
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
    /// The inline image itself — capped at 256KB by the compressor, but the
    /// single largest thing a backup carries.
    pub blob: Option<Vec<u8>>,
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    /// Absent from blobs written before voice notes; they held no voice rows.
    #[serde(default)]
    pub duration_ms: u32,
}

pub fn dump_all() -> Vec<MediaBackupRow> {
    let conn = MESSAGES_DB.lock();
    let Ok(mut stmt) = conn.prepare("SELECT * FROM message_media") else { return Vec::new() };
    stmt.query_map([], |r| {
        let conv: Vec<u8> = r.get("conversation_id")?;
        let did: Vec<u8> = r.get("dispatch_id")?;
        Ok(MediaBackupRow {
            conversation_id: conv.try_into().unwrap_or([0u8; 16]),
            dispatch_id: did.try_into().unwrap_or([0u8; 16]),
            kind: r.get("kind")?,
            group_id: r.get("group_id")?,
            mime: r.get("mime")?,
            name: r.get("name")?,
            size: r.get("size")?,
            width: r.get("width")?,
            height: r.get("height")?,
            blob: r.get("blob")?,
            thumb: r.get("thumb")?,
            file_id: r.get("file_id")?,
            duration_ms: r.get("duration_ms")?,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// Restore dumped media. `INSERT OR IGNORE` — a picture we already hold wins
/// over the snapshot's copy of it.
pub fn import_rows(rows: &[MediaBackupRow]) -> Result<usize> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    let mut n = 0usize;
    for r in rows {
        n += tx.execute(
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
    tx.commit()?;
    Ok(n)
}
