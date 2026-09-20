//! Pictures people assert about themselves, learned inside a shared chat.
//!
//! The companion of [`peer_name`](crate::data::peer_name), with one difference
//! in standing: the address book holds a *name* the local user chose, but never
//! a picture, so what a person says they look like is the only account there
//! is. Stored as the AVIF bytes they sent, capped at [`MAX_AVATAR_BYTES`] so a
//! hostile member cannot park a payload here under the name of a face.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::bail;
use common::proto::mls_wire::MAX_AVATAR_BYTES;
use rusqlite::Connection;

use crate::db::messages::MESSAGES_DB;
use crate::utils::systime;

/// Moves whenever any picture changes, ours or theirs. The DB doorbell only
/// says that *something* in the messages DB committed, which is every message;
/// a client that caches decoded pictures compares this instead, and re-decodes
/// only when a picture actually moved.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

pub(crate) fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Refuse anything that is not a plausibly small AVIF file. The size cap is
/// the wire contract; the `ftyp` box is the cheapest tell that the bytes are
/// an image container at all rather than whatever a sender chose to label one.
pub fn check_avif(bytes: &[u8]) -> Result<()> {
    if bytes.len() > MAX_AVATAR_BYTES {
        bail!("picture is {} bytes, over the {MAX_AVATAR_BYTES} cap", bytes.len());
    }
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        bail!("picture is not an AVIF file");
    }
    Ok(())
}

/// Record what `who` looks like. Last assertion wins: a new picture is just
/// the same person showing something new.
pub fn put(who: &[u8; 32], avif: &[u8]) -> Result<()> {
    let conn = MESSAGES_DB.lock();
    put_tx(&conn, who, avif)
}

pub fn put_tx(conn: &Connection, who: &[u8; 32], avif: &[u8]) -> Result<()> {
    check_avif(avif)?;
    conn.execute(
        "INSERT INTO peer_avatars (ipk, avif, updated_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(ipk) DO UPDATE SET avif = excluded.avif, updated_at = excluded.updated_at",
        (who.as_slice(), avif, systime().as_secs()),
    )?;
    bump_generation();
    Ok(())
}

/// `who` took their picture down: forget it, so we stop showing a face they
/// chose to remove.
pub fn clear(who: &[u8; 32]) -> Result<()> {
    let conn = MESSAGES_DB.lock();
    clear_tx(&conn, who)
}

pub fn clear_tx(conn: &Connection, who: &[u8; 32]) -> Result<()> {
    conn.execute("DELETE FROM peer_avatars WHERE ipk = ?1", [who.as_slice()])?;
    bump_generation();
    Ok(())
}

pub fn get(who: &[u8; 32]) -> Option<Vec<u8>> {
    let conn = MESSAGES_DB.lock();
    get_tx(&conn, who)
}

pub fn get_tx(conn: &Connection, who: &[u8; 32]) -> Option<Vec<u8>> {
    conn.query_row("SELECT avif FROM peer_avatars WHERE ipk = ?1", [who.as_slice()], |r| r.get(0))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::messages::open_in_memory;

    /// The smallest thing the gate accepts: an ISOBMFF `ftyp` box and a byte
    /// of nothing after it. Real AVIF follows; the store never decodes.
    fn avif_like(fill: u8) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x1c, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f'];
        v.push(fill);
        v
    }

    #[test]
    fn a_picture_round_trips_and_the_latest_one_wins() {
        let conn = open_in_memory();
        let who = [7u8; 32];
        assert_eq!(get_tx(&conn, &who), None, "nothing known yet");

        put_tx(&conn, &who, &avif_like(1)).expect("first");
        assert_eq!(get_tx(&conn, &who), Some(avif_like(1)));

        put_tx(&conn, &who, &avif_like(2)).expect("replace");
        assert_eq!(get_tx(&conn, &who), Some(avif_like(2)), "a new picture replaces the old");

        clear_tx(&conn, &who).expect("clear");
        assert_eq!(get_tx(&conn, &who), None, "taken down means gone");
        clear_tx(&conn, &who).expect("clearing nothing is not an error");
    }

    #[test]
    fn the_gate_refuses_what_is_not_a_small_avif() {
        let conn = open_in_memory();
        let who = [8u8; 32];

        let mut oversized = avif_like(0);
        oversized.resize(MAX_AVATAR_BYTES + 1, 0);
        assert!(put_tx(&conn, &who, &oversized).is_err(), "over the cap");

        assert!(put_tx(&conn, &who, b"not an image at all").is_err(), "no ftyp box");
        assert!(put_tx(&conn, &who, &[]).is_err(), "empty");

        assert_eq!(get_tx(&conn, &who), None, "a refused picture leaves no row");

        let mut at_cap = avif_like(0);
        at_cap.resize(MAX_AVATAR_BYTES, 0);
        put_tx(&conn, &who, &at_cap).expect("exactly the cap is allowed");
    }
}
