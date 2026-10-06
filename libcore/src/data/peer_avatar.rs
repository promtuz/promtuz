//! Pictures people assert about themselves, stored as the AVIF bytes they sent.

use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::bail;
use common::proto::mls_wire::MAX_AVATAR_BYTES;
use rusqlite::Connection;

use crate::state::core;

/// Moves whenever any picture changes. The DB doorbell fires on every commit, so a client caching
/// decoded pictures compares this instead.
pub fn generation() -> u64 {
    core().avatar_generation.load(Ordering::Relaxed)
}

/// Call after releasing the DB lock: the commit hook fired before the generation moved, and the
/// identity DB has no hook at all.
pub(crate) fn notify_changed() {
    core().avatar_generation.fetch_add(1, Ordering::Relaxed);
    if let Some(events) = core().events.get() {
        events.on_db_changed(vec!["peer_avatars".into()]);
    }
}

/// The size cap is the wire contract; the `ftyp` box is the cheapest tell that the bytes are an
/// image container at all.
pub fn check_avif(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_AVATAR_BYTES {
        bail!("picture is {} bytes, over the {MAX_AVATAR_BYTES} cap", bytes.len());
    }
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        bail!("picture is not an AVIF file");
    }
    Ok(())
}

pub fn get(who: &[u8; 32]) -> Option<Vec<u8>> {
    let conn = core().db.messages().lock();
    get_tx(&conn, who)
}

pub fn get_tx(conn: &Connection, who: &[u8; 32]) -> Option<Vec<u8>> {
    conn.query_row("SELECT avif FROM peer_avatars WHERE ipk = ?1", [who.as_slice()], |r| r.get(0))
        .ok()
        .flatten()
}
