//! The sticker packs this device keeps: every pack in the picker, with its
//! token and roster, plus the recents row and the cached store directory.

use anyhow::{Result, ensure};
use common::proto::client_res::StoreDescriptor;
use common::proto::pack::{Packer, Unpacker};
use rusqlite::Connection;
use rusqlite::params;

use crate::db::all;
use crate::db::from_row;
use crate::db::one;
use crate::state::core;

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

from_row!(PackRow { pack_id, store_id, token, creator, version, name, added_at });
from_row!(StickerRow { pack_id, sticker_id, position, width, height });

fn packs_tx(conn: &Connection) -> rusqlite::Result<Vec<PackRow>> {
    all(conn, "SELECT * FROM sticker_packs ORDER BY added_at, pack_id", [], PackRow::from_row)
}

pub fn list_packs() -> Vec<PackRow> {
    packs_tx(&core().db.messages().lock()).unwrap_or_default()
}

pub fn get_pack(pack: &[u8; 16]) -> Option<PackRow> {
    pack_at(&core().db.messages().lock(), pack).ok().flatten()
}

pub fn stickers_of(pack: &[u8; 16]) -> rusqlite::Result<Vec<StickerRow>> {
    stickers_of_tx(&core().db.messages().lock(), pack)
}

fn stickers_of_tx(conn: &Connection, pack: &[u8; 16]) -> rusqlite::Result<Vec<StickerRow>> {
    all(
        conn,
        "SELECT * FROM stickers WHERE pack_id = ?1 ORDER BY position",
        [pack.as_slice()],
        StickerRow::from_row,
    )
}

/// Install explicitly. Existing packs never move backward or change identity.
pub fn upsert_pack(row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    write_pack(&tx, row, stickers)?;
    tx.commit()?;
    Ok(())
}

/// Apply a refresh only if the pack has not changed since the request began.
pub fn refresh_pack(expected: &PackRow, row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    refresh_pack_at(&tx, expected, row, stickers)?;
    tx.commit()?;
    Ok(())
}

fn refresh_pack_at(
    conn: &Connection, expected: &PackRow, row: &PackRow, stickers: &[StickerRow],
) -> Result<()> {
    if pack_at(conn, &row.pack_id)?.as_ref() == Some(expected) {
        write_pack(conn, row, stickers)?;
    }
    Ok(())
}

fn pack_at(conn: &Connection, id: &[u8; 16]) -> rusqlite::Result<Option<PackRow>> {
    one(conn, "SELECT * FROM sticker_packs WHERE pack_id = ?1", [id], PackRow::from_row)
}

fn write_pack(conn: &Connection, row: &PackRow, stickers: &[StickerRow]) -> Result<()> {
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
    core().db.messages().lock().execute(
        "INSERT INTO sticker_uploads (pack_id, payload) VALUES (?1, ?2) ON CONFLICT(pack_id) DO UPDATE SET payload = excluded.payload",
        params![upload.pack.pack_id, upload.ser()?],
    )?;
    Ok(())
}

pub fn pending_uploads() -> Result<Vec<PendingUpload>> {
    let bytes = all(
        &core().db.messages().lock(),
        "SELECT payload FROM sticker_uploads ORDER BY pack_id",
        [],
        |r| r.get::<_, Vec<u8>>(0),
    )?;
    bytes.into_iter().map(|b| PendingUpload::deser(&b).map_err(Into::into)).collect()
}

pub fn finish_upload(upload: &PendingUpload) -> Result<()> {
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    write_pack(&tx, &upload.pack, &upload.stickers)?;
    tx.execute("DELETE FROM sticker_uploads WHERE pack_id = ?1", [upload.pack.pack_id])?;
    tx.commit()?;
    Ok(())
}

pub fn remove_pack(pack: &[u8; 16]) -> Result<()> {
    let mut db = core().db.messages().lock();
    let tx = db.transaction()?;
    tx.execute("DELETE FROM sticker_recents WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.execute("DELETE FROM stickers WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.execute("DELETE FROM sticker_packs WHERE pack_id = ?1", [pack.as_slice()])?;
    tx.commit()?;
    Ok(())
}

pub fn touch_recent(pack: &[u8; 16], sticker: &[u8; 32], now: u64) -> Result<()> {
    core().db.messages().lock().execute(
        "INSERT INTO sticker_recents (pack_id, sticker_id, used_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(pack_id, sticker_id) DO UPDATE SET used_at = excluded.used_at",
        params![pack.as_slice(), sticker.as_slice(), now],
    )?;
    Ok(())
}

pub fn recents(limit: u32) -> Vec<StickerRow> {
    all(
        &core().db.messages().lock(),
        "SELECT s.* FROM sticker_recents r \
         JOIN stickers s ON s.pack_id = r.pack_id AND s.sticker_id = r.sticker_id \
         ORDER BY r.used_at DESC LIMIT ?1",
        [limit],
        StickerRow::from_row,
    )
    .unwrap_or_default()
}

pub fn cached_store(id: u16) -> Option<String> {
    one(&core().db.network().lock(), "SELECT base_url FROM stores WHERE id = ?1", [id], |r| r.get(0))
        .ok()
        .flatten()
}

pub fn cache_stores(stores: &[StoreDescriptor]) {
    let mut conn = core().db.network().lock();
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PackBackup {
    pub pack: PackRow,
    pub stickers: Vec<StickerRow>,
}

pub fn dump_all_tx(conn: &Connection) -> rusqlite::Result<Vec<PackBackup>> {
    packs_tx(conn)?
        .into_iter()
        .map(|pack| Ok(PackBackup { stickers: stickers_of_tx(conn, &pack.pack_id)?, pack }))
        .collect()
}

pub fn import_rows_tx(conn: &Connection, rows: &[PackBackup]) -> Result<usize> {
    let mut n = 0usize;
    for r in rows {
        if pack_at(conn, &r.pack.pack_id)?.is_none() {
            write_pack(conn, &r.pack, &r.stickers)?;
            n += 1;
        }
    }
    Ok(n)
}
