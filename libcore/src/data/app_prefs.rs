//! Settings that belong to the user rather than the device, so they ride the backup blob.

use anyhow::Result;
use rusqlite::Connection;

use crate::db::all;
use crate::state::core;

pub fn get(key: &str) -> Option<String> {
    get_tx(&core().db.messages().lock(), key)
}

pub fn get_tx(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM app_prefs WHERE key = ?1", [key], |r| r.get(0)).ok()
}

pub fn set(key: &str, value: &str) -> Result<()> {
    set_tx(&core().db.messages().lock(), key, value)
}

pub fn set_tx(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_prefs (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

pub fn remove(key: &str) -> Result<()> {
    remove_tx(&core().db.messages().lock(), key)
}

pub fn remove_tx(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM app_prefs WHERE key = ?1", [key])?;
    Ok(())
}

pub fn with_prefix(prefix: &str) -> Vec<(String, String)> {
    with_prefix_tx(&core().db.messages().lock(), prefix)
}

pub fn with_prefix_tx(conn: &Connection, prefix: &str) -> Vec<(String, String)> {
    all(
        conn,
        "SELECT key, value FROM app_prefs WHERE substr(key, 1, length(?1)) = ?1",
        [prefix],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap_or_default()
}

pub fn dump_all_tx(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    all(conn, "SELECT key, value FROM app_prefs", [], |r| Ok((r.get(0)?, r.get(1)?)))
}

pub fn import_rows_tx(conn: &Connection, rows: &[(String, String)]) -> Result<usize> {
    let mut n = 0usize;
    for (k, v) in rows {
        n += conn.execute("INSERT OR IGNORE INTO app_prefs (key, value) VALUES (?1, ?2)", (k, v))?;
    }
    Ok(n)
}
