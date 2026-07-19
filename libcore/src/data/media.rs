//! Per-message media metadata (Image inline bytes / Attachment thumb + file_id),
//! keyed by (peer_ipk, dispatch_id). The caption itself lives on messages.content.
use anyhow::Result;
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
    db.execute(
        "INSERT OR REPLACE INTO message_media
         (peer_ipk,dispatch_id,kind,group_id,mime,name,size,width,height,blob,thumb,file_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        rusqlite::params![peer.as_slice(), dispatch_id.as_slice(), r.kind, r.group_id,
            r.mime, r.name, r.size, r.width, r.height, r.blob, r.thumb, r.file_id],
    )?;
    Ok(())
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
}
