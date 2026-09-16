//! The sticker packs this device keeps: every pack in the picker, with its
//! token and roster, plus the recents row and the cached store directory.

use anyhow::{Result, ensure};
use common::proto::client_res::StoreDescriptor;
use common::proto::pack::{Packer, Unpacker};
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::db::messages::MESSAGES_DB;
use crate::db::network::NETWORK_DB;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PackRow {
    pub pack_id: [u8; 16],
    pub store_id: u16,
    pub token: [u8; 32],
    /// Pinned on first sight: a later manifest under another key is refused.
    pub creator: [u8; 32],
    pub version: u32,
    pub name: String,
    pub added_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StickerRow {
    pub pack_id: [u8; 16],
    pub sticker_id: [u8; 32],
    pub position: u32,
    pub width: u16,
    pub height: u16,
}

fn pack_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<PackRow> {
    let pack_id: Vec<u8> = r.get(0)?;
    let token: Vec<u8> = r.get(2)?;
    let creator: Vec<u8> = r.get(3)?;
    Ok(PackRow {
        pack_id: pack_id.try_into().unwrap_or([0; 16]),
        store_id: r.get(1)?,
        token: token.try_into().unwrap_or([0; 32]),
        creator: creator.try_into().unwrap_or([0; 32]),
        version: r.get(4)?,
        name: r.get(5)?,
        added_at: r.get(6)?,
    })
}

fn sticker_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<StickerRow> {
    let pack_id: Vec<u8> = r.get(0)?;
    let sticker_id: Vec<u8> = r.get(1)?;
    Ok(StickerRow {
        pack_id: pack_id.try_into().unwrap_or([0; 16]),
        sticker_id: sticker_id.try_into().unwrap_or([0; 32]),
        position: r.get(2)?,
        width: r.get(3)?,
        height: r.get(4)?,
    })
}

const PACK_COLS: &str = "pack_id, store_id, token, creator, version, name, added_at";
const STICKER_COLS: &str = "pack_id, sticker_id, position, width, height";

/// Every kept pack, oldest first — the picker's tab order.
pub fn list_packs() -> Vec<PackRow> {
    let db = MESSAGES_DB.lock();
    let Ok(mut stmt) =
        db.prepare(&format!("SELECT {PACK_COLS} FROM sticker_packs ORDER BY added_at, pack_id"))
    else {
        return Vec::new();
    };
    stmt.query_map([], pack_from).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

pub fn get_pack(pack: &[u8; 16]) -> Option<PackRow> {
    let db = MESSAGES_DB.lock();
    db.query_row(
        &format!("SELECT {PACK_COLS} FROM sticker_packs WHERE pack_id = ?1"),
        [pack.as_slice()],
        pack_from,
    )
    .optional()
    .ok()
    .flatten()
}

pub fn stickers_of(pack: &[u8; 16]) -> Vec<StickerRow> {
    let db = MESSAGES_DB.lock();
    stickers_of_tx(&db, pack)
}

fn stickers_of_tx(conn: &rusqlite::Connection, pack: &[u8; 16]) -> Vec<StickerRow> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {STICKER_COLS} FROM stickers WHERE pack_id = ?1 ORDER BY position"
    )) else {
        return Vec::new();
    };
    stmt.query_map([pack.as_slice()], sticker_from)
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Install explicitly. Existing packs never move backward or change identity.
pub fn upsert_pack(row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    write_pack(&tx, row, stickers)?;
    tx.commit()?;
    Ok(())
}

/// Apply a refresh only if the pack has not changed since the request began.
pub fn refresh_pack(expected: &PackRow, row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    refresh_pack_at(&tx, expected, row, stickers)?;
    tx.commit()?;
    Ok(())
}

fn refresh_pack_at(
    conn: &rusqlite::Connection, expected: &PackRow, row: &PackRow, stickers: &[StickerRow],
) -> Result<()> {
    if pack_at(conn, &row.pack_id)?.as_ref() == Some(expected) {
        write_pack(conn, row, stickers)?;
    }
    Ok(())
}

fn pack_at(conn: &rusqlite::Connection, id: &[u8; 16]) -> Result<Option<PackRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {PACK_COLS} FROM sticker_packs WHERE pack_id = ?1"),
            [id],
            pack_from,
        )
        .optional()?)
}

fn write_pack(conn: &rusqlite::Connection, row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
    if let Some(current) = pack_at(conn, &row.pack_id)? {
        ensure!(
            current.creator == row.creator
                && current.token == row.token
                && current.store_id == row.store_id,
            "pack identity changed"
        );
        if current.version >= row.version {
            return Ok(());
        }
    }
    conn.execute(
        "INSERT INTO sticker_packs (pack_id, store_id, token, creator, version, name, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(pack_id) DO UPDATE SET version = excluded.version, name = excluded.name",
        params![
            row.pack_id,
            row.store_id,
            row.token,
            row.creator,
            row.version,
            row.name,
            row.added_at
        ],
    )?;
    conn.execute("DELETE FROM stickers WHERE pack_id = ?1", [row.pack_id])?;
    for s in stickers {
        conn.execute(
            "INSERT INTO stickers (pack_id, sticker_id, position, width, height) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![row.pack_id, s.sticker_id, s.position, s.width, s.height],
        )?;
    }
    Ok(())
}

/// Persist the exact signed requests before sending any of them.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingUpload {
    pub pack: PackRow,
    pub stickers: Vec<StickerRow>,
    pub requests: Vec<common::proto::sticker::StoreRequest>,
}

pub fn save_upload(upload: &PendingUpload) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "INSERT INTO sticker_uploads (pack_id, payload) VALUES (?1, ?2) ON CONFLICT(pack_id) DO UPDATE SET payload = excluded.payload",
        params![upload.pack.pack_id, upload.ser()?],
    )?;
    Ok(())
}

pub fn pending_uploads() -> Result<Vec<PendingUpload>> {
    let conn = MESSAGES_DB.lock();
    let mut stmt = conn.prepare("SELECT payload FROM sticker_uploads ORDER BY pack_id")?;
    let bytes =
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    bytes.into_iter().map(|b| PendingUpload::deser(&b).map_err(Into::into)).collect()
}

pub fn finish_upload(upload: &PendingUpload) -> Result<()> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    write_pack(&tx, &upload.pack, &upload.stickers)?;
    tx.execute("DELETE FROM sticker_uploads WHERE pack_id = ?1", [upload.pack.pack_id])?;
    tx.commit()?;
    Ok(())
}

pub fn remove_pack(pack: &[u8; 16]) -> Result<()> {
    let mut db = MESSAGES_DB.lock();
    let tx = db.transaction()?;
    tx.execute("DELETE FROM sticker_recents WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.execute("DELETE FROM stickers WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.execute("DELETE FROM sticker_packs WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.commit()?;
    Ok(())
}

pub fn touch_recent(pack: &[u8; 16], sticker: &[u8; 32], now: u64) -> Result<()> {
    MESSAGES_DB.lock().execute(
        "INSERT INTO sticker_recents (pack_id, sticker_id, used_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(pack_id, sticker_id) DO UPDATE SET used_at = excluded.used_at",
        params![pack.as_slice(), sticker.as_slice(), now],
    )?;
    Ok(())
}

/// Most recently sent first, only from packs still kept.
pub fn recents(limit: u32) -> Vec<StickerRow> {
    let db = MESSAGES_DB.lock();
    let Ok(mut stmt) = db.prepare(
        "SELECT s.pack_id, s.sticker_id, s.position, s.width, s.height \
         FROM sticker_recents r \
         JOIN stickers s ON s.pack_id = r.pack_id AND s.sticker_id = r.sticker_id \
         ORDER BY r.used_at DESC LIMIT ?1",
    ) else {
        return Vec::new();
    };
    stmt.query_map([limit], sticker_from).map(|rows| rows.flatten().collect()).unwrap_or_default()
}

pub fn cached_store(id: u16) -> Option<String> {
    let conn = NETWORK_DB.lock();
    conn.query_row("SELECT base_url FROM stores WHERE id = ?1", [id], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

/// Replace the cached directory with what the resolver just said.
pub fn cache_stores(stores: &[StoreDescriptor]) {
    let mut conn = NETWORK_DB.lock();
    let Ok(tx) = conn.transaction() else { return };
    let _ = tx.execute("DELETE FROM stores", []);
    for s in stores {
        let _ = tx.execute(
            "INSERT INTO stores (id, base_url) VALUES (?1, ?2)",
            params![s.id, s.base_url.trim_end_matches('/')],
        );
    }
    let _ = tx.commit();
}

/// A pack with its roster, for the backup.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackBackup {
    pub pack: PackRow,
    pub stickers: Vec<StickerRow>,
}

pub fn dump_all() -> Vec<PackBackup> {
    let packs = list_packs();
    let db = MESSAGES_DB.lock();
    packs
        .into_iter()
        .map(|pack| PackBackup { stickers: stickers_of_tx(&db, &pack.pack_id), pack })
        .collect()
}

/// Restore missing packs without overwriting installed versions.
pub fn import_rows(rows: &[PackBackup]) -> Result<usize> {
    let mut n = 0usize;
    for r in rows {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        if pack_at(&tx, &r.pack.pack_id)?.is_none() {
            write_pack(&tx, &r.pack, &r.stickers)?;
            n += 1;
        }
        tx.commit()?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_refresh_preserves_removal_and_newer_versions() {
        let conn = crate::db::messages::open_in_memory();
        let original = PackRow {
            pack_id: [1; 16],
            store_id: 1,
            token: [2; 32],
            creator: [3; 32],
            version: 1,
            name: "Pack".into(),
            added_at: 1,
        };
        let old_response = PackRow { version: 2, ..original.clone() };
        let new_response = PackRow { version: 3, ..original.clone() };
        write_pack(&conn, &original, &[]).unwrap();
        write_pack(&conn, &new_response, &[]).unwrap();
        refresh_pack_at(&conn, &original, &old_response, &[]).unwrap();
        assert_eq!(pack_at(&conn, &original.pack_id).unwrap().unwrap().version, 3);
        conn.execute("DELETE FROM sticker_packs", []).unwrap();
        refresh_pack_at(&conn, &original, &old_response, &[]).unwrap();
        assert!(pack_at(&conn, &original.pack_id).unwrap().is_none());
    }
}
