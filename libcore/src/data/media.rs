//! Per-message media metadata (Image inline bytes / Attachment thumb + file_id),
//! keyed by (peer_ipk, dispatch_id). The caption itself lives on messages.content.
use anyhow::Result;
use rusqlite::OptionalExtension;
use crate::db::messages::MESSAGES_DB;

pub const KIND_IMAGE: u8 = 1;
pub const KIND_ATTACHMENT: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaRow {
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
}

pub fn save(peer: &[u8; 32], dispatch_id: &[u8; 16], r: &MediaRow) -> Result<()> {
    let db = MESSAGES_DB.lock();
    save_tx(&db, peer, dispatch_id, r)
}

/// Transaction-scoped [`save`]: writes the media row against a caller-supplied
/// connection so it can share one transaction with the caption insert.
pub fn save_tx(
    conn: &rusqlite::Connection, peer: &[u8; 32], dispatch_id: &[u8; 16], r: &MediaRow,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO message_media
         (peer_ipk,dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice(), r.kind, r.group_id,
            r.mime, r.name, r.size, r.width, r.height, r.blob, r.thumb, r.file_id],
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
    peer: &[u8; 32], dispatch_id: &[u8; 16], caption: &str, timestamp: u64, r: &MediaRow,
) -> Result<Option<crate::data::message::Message>> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    let saved = crate::data::message::Message::save_incoming_tx(
        &tx, *peer, dispatch_id, caption, timestamp, None,
    )?;
    if saved.is_some() {
        save_tx(&tx, peer, dispatch_id, r)?;
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
    peer: &[u8; 32], caption: &str, reply_to: Option<[u8; 16]>, r: &MediaRow,
) -> Result<crate::data::message::Message> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    let msg = crate::data::message::Message::save_outgoing_tx(&tx, *peer, caption, reply_to)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .expect("save_outgoing mints a dispatch_id")
        .try_into()
        .expect("dispatch_id is 16 bytes");
    save_tx(&tx, peer, &did, r)?;
    tx.commit()?;
    Ok(msg)
}

/// Fill an outgoing image's compressed bytes + final size/dims once encoding
/// finishes (the placeholder row was inserted with a null blob so the bubble
/// could show instantly).
pub fn set_blob(
    peer: &[u8; 32], dispatch_id: &[u8; 16], blob: &[u8], width: u32, height: u32,
) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "UPDATE message_media SET blob=?3, size=?4, width=?5, height=?6
         WHERE peer_ipk=?1 AND dispatch_id=?2",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice(), blob, blob.len() as u64,
            width, height],
    )?;
    Ok(())
}

/// Fill an outgoing attachment's content-addressed file_id once the manifest
/// pass finishes (placeholder inserted with a null file_id).
pub fn set_file_id(peer: &[u8; 32], dispatch_id: &[u8; 16], file_id: &[u8; 32]) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "UPDATE message_media SET file_id=?3 WHERE peer_ipk=?1 AND dispatch_id=?2",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice(), file_id.as_slice()],
    )?;
    Ok(())
}

/// Remove an outgoing media message wholesale — caption row + media side-row —
/// when the heavy prep (compress / manifest) fails before the send ever
/// started, so no dead placeholder bubble lingers. One transaction.
pub fn discard_outgoing(peer: &[u8; 32], dispatch_id: &[u8; 16]) -> Result<()> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    tx.execute(
        "DELETE FROM message_media WHERE peer_ipk=?1 AND dispatch_id=?2",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice()],
    )?;
    tx.execute(
        "DELETE FROM messages WHERE peer_ipk=?1 AND dispatch_id=?2",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice()],
    )?;
    tx.commit()?;
    Ok(())
}

/// The media side-row for one message (by peer + dispatch_id), or `None` if
/// the message carries no media. Lets the send-retry path rebuild the original
/// media payload instead of downgrading it to bare text.
pub fn get(peer: &[u8; 32], dispatch_id: &[u8; 16]) -> Result<Option<MediaRow>> {
    let db = MESSAGES_DB.lock();
    db.query_row(
        "SELECT kind,group_id,mime,name,size,width,height,blob,thumb,file_id
         FROM message_media WHERE peer_ipk=?1 AND dispatch_id=?2",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice()],
        |row| Ok(MediaRow {
            kind: row.get(0)?, group_id: row.get(1)?, mime: row.get(2)?, name: row.get(3)?,
            size: row.get(4)?, width: row.get(5)?, height: row.get(6)?,
            blob: row.get(7)?, thumb: row.get(8)?, file_id: row.get(9)?,
        }),
    )
    .optional()
    .map_err(Into::into)
}

/// The sender to dial for an incoming attachment and the size they advertised
/// in the offer — the pull rejects a manifest whose `total_size` belies it.
/// Restricted to the INCOMING row (`m.outgoing = 0`): if we both received and
/// re-sent the same content-addressed file, the outgoing row's peer is our own
/// recipient (who serves `Gone`), not the sender we must pull from.
pub fn attachment_offer(file_id: &[u8; 32]) -> Result<Option<([u8; 32], u64)>> {
    let db = MESSAGES_DB.lock();
    db.query_row(
        "SELECT mm.peer_ipk, mm.size FROM message_media mm
           JOIN messages m ON m.peer_ipk = mm.peer_ipk AND m.dispatch_id = mm.dispatch_id
         WHERE mm.file_id = ?1 AND m.outgoing = 0 LIMIT 1",
        [file_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

pub fn for_peer(peer: &[u8; 32]) -> Result<Vec<([u8; 16], MediaRow)>> {
    let db = MESSAGES_DB.lock();
    let mut stmt = db.prepare(
        "SELECT dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id
         FROM message_media WHERE peer_ipk=?1")?;
    let rows = stmt.query_map([peer.as_slice()], |row| {
        let did: Vec<u8> = row.get(0)?;
        let mut d = [0u8; 16]; d.copy_from_slice(&did);
        Ok((d, MediaRow {
            kind: row.get(1)?, group_id: row.get(2)?, mime: row.get(3)?, name: row.get(4)?,
            size: row.get(5)?, width: row.get(6)?, height: row.get(7)?,
            blob: row.get(8)?, thumb: row.get(9)?, file_id: row.get(10)?,
        }))
    })?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
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
        let peer = [3u8; 32]; let did = [4u8; 16];
        let row = MediaRow { kind: KIND_IMAGE, group_id: Some(vec![1u8;16]),
            mime: "image/avif".into(), name: "".into(), size: 3, width: 4, height: 3,
            blob: Some(vec![9,9,9]), thumb: None, file_id: None };
        save(&peer, &did, &row).unwrap();
        let got = for_peer(&peer).unwrap();
        assert!(got.iter().any(|(d, r)| *d == did && r.blob == row.blob && r.kind == KIND_IMAGE));
    }

    /// Two-phase optimistic send: a placeholder saved with a null blob is
    /// filled in place by `set_blob` once the encode finishes.
    #[test]
    fn set_blob_fills_placeholder() {
        let dir = std::env::temp_dir().join("promtuz-media-setblob-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [0x21u8; 32];
        let did = [0x22u8; 16];
        let row = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: "".into(), size: 0, width: 4, height: 3,
            blob: None, thumb: None, file_id: None };
        save(&peer, &did, &row).unwrap();
        assert!(get(&peer, &did).unwrap().unwrap().blob.is_none());

        set_blob(&peer, &did, &[7, 8, 9], 2, 2).unwrap();
        let got = get(&peer, &did).unwrap().unwrap();
        assert_eq!(got.blob, Some(vec![7, 8, 9]));
        assert_eq!(got.size, 3);
        assert_eq!((got.width, got.height), (2, 2));
    }

    /// A failed prep must not leave a dead placeholder bubble: both the
    /// caption row and the media side-row go.
    #[test]
    fn discard_outgoing_removes_caption_and_media() {
        let dir = std::env::temp_dir().join("promtuz-media-discard-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [0x23u8; 32];
        let row = MediaRow { kind: KIND_ATTACHMENT, group_id: None,
            mime: "application/pdf".into(), name: "a.pdf".into(), size: 9,
            width: 0, height: 0, blob: None, thumb: None, file_id: None };
        let msg = save_outgoing_with_media(&peer, "cap", None, &row).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();
        assert!(get(&peer, &did).unwrap().is_some());

        discard_outgoing(&peer, &did).unwrap();
        assert!(get(&peer, &did).unwrap().is_none(), "media side-row gone");
        assert!(
            crate::data::message::Message::get_by_dispatch(&peer, &did).is_none(),
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
        let peer = [5u8; 32];

        // Happy path: both rows land in one committed transaction.
        let did = [6u8; 16];
        let media = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: String::new(), size: 3, width: 1, height: 1,
            blob: Some(vec![1, 2, 3]), thumb: None, file_id: None };
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::save_incoming_tx(&tx, peer, &did, "cap", 100, None).unwrap().is_some());
            save_tx(&tx, &peer, &did, &media).unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did.as_slice()), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_media WHERE dispatch_id=?1", did.as_slice()), 1);

        // Rollback path: a failing media write undoes the caption row already
        // inserted in the same transaction.
        let did2 = [7u8; 16];
        {
            let tx = conn.transaction().unwrap();
            assert!(Message::save_incoming_tx(&tx, peer, &did2, "cap2", 100, None).unwrap().is_some());
            let bad = tx.execute(
                "INSERT INTO message_media (peer_ipk,dispatch_id,kind,mime) VALUES (?1,?2,NULL,?3)",
                rusqlite::params![peer.as_slice(), did2.as_slice(), "image/avif"],
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
        let peer = [8u8; 32];
        let media = MediaRow { kind: KIND_IMAGE, group_id: None, mime: "image/avif".into(),
            name: String::new(), size: 3, width: 4, height: 3,
            blob: Some(vec![1, 2, 3]), thumb: None, file_id: None };

        // Happy path: caption + media land in one committed transaction.
        let did: [u8; 16] = {
            let tx = conn.transaction().unwrap();
            let msg = Message::save_outgoing_tx(&tx, peer, "cap", None).unwrap();
            let d: [u8; 16] = msg.inner.dispatch_id.unwrap().try_into().unwrap();
            save_tx(&tx, &peer, &d, &media).unwrap();
            tx.commit().unwrap();
            d
        };
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did.as_slice()), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_media WHERE dispatch_id=?1", did.as_slice()), 1);

        // Rollback path: a failing media write undoes the caption already
        // inserted in the same transaction.
        let did2: [u8; 16] = {
            let tx = conn.transaction().unwrap();
            let msg = Message::save_outgoing_tx(&tx, peer, "cap2", None).unwrap();
            let d: [u8; 16] = msg.inner.dispatch_id.unwrap().try_into().unwrap();
            let bad = tx.execute(
                "INSERT INTO message_media (peer_ipk,dispatch_id,kind,mime) VALUES (?1,?2,NULL,?3)",
                rusqlite::params![peer.as_slice(), d.as_slice(), "image/avif"],
            );
            assert!(bad.is_err(), "NULL kind must violate NOT NULL");
            d // tx dropped without commit → rollback
        };
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE dispatch_id=?1", did2.as_slice()), 0,
            "media failure rolled back the caption — no orphan");
    }
}
