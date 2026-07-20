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
