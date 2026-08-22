//! App-wide settings that belong to the user rather than the device.
//!
//! They lived in SharedPreferences, which `backup_rules.xml` does not ship —
//! it carries the encrypted blob and nothing else — so every reinstall silently
//! reset them. Here they ride the blob, and cost iOS nothing to reuse.
//!
//! Stringly-typed on purpose: each value is read once, by a screen that already
//! knows what it means.

use anyhow::Result;

use crate::db::messages::MESSAGES_DB;

pub fn get(key: &str) -> Option<String> {
    let conn = MESSAGES_DB.lock();
    conn.query_row("SELECT value FROM app_prefs WHERE key = ?1", [key], |r| r.get(0)).ok()
}

pub fn set(key: &str, value: &str) -> Result<()> {
    let conn = MESSAGES_DB.lock();
    conn.execute(
        "INSERT INTO app_prefs (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )?;
    Ok(())
}

/// Every setting, for the backup snapshot.
pub fn dump_all() -> Vec<(String, String)> {
    let conn = MESSAGES_DB.lock();
    conn.prepare("SELECT key, value FROM app_prefs")
        .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|r| r.flatten().collect()))
        .unwrap_or_default()
}

/// Restore settings. `INSERT OR IGNORE`: a setting the user has already changed
/// on this device outranks the snapshot's memory of it.
pub fn import_rows(rows: &[(String, String)]) -> Result<usize> {
    let mut conn = MESSAGES_DB.lock();
    let tx = conn.transaction()?;
    let mut n = 0usize;
    for (k, v) in rows {
        n += tx.execute("INSERT OR IGNORE INTO app_prefs (key, value) VALUES (?1, ?2)", (k, v))?;
    }
    tx.commit()?;
    Ok(n)
}
