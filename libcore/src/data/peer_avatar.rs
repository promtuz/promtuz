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

/// Called after a successful write and after releasing its DB lock. SQLite's
/// commit hook fires earlier, so publish a second doorbell after the generation
/// moves. Own-profile writes need this too: the identity DB has no change hook.
pub(crate) fn notify_changed() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
    if let Some(events) = crate::platform::EVENTS.get() {
        events.on_db_changed(vec!["peer_avatars".into()]);
    }
}

#[derive(Clone, Debug)]
pub struct AvatarUpdate {
    pub revision: u64,
    pub avif: Option<Vec<u8>>,
}

impl AvatarUpdate {
    pub fn into_payload(self) -> common::proto::mls_wire::AppPayload {
        common::proto::mls_wire::AppPayload::Avatar { revision: self.revision, avif: self.avif }
    }
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

/// Apply only a newer owner-issued revision, regardless of delivery order or
/// which shared chat carried it. A NULL picture is a durable removal tombstone.
pub fn apply(who: &[u8; 32], update: &AvatarUpdate) -> Result<()> {
    let changed = {
        let conn = MESSAGES_DB.lock();
        apply_tx(&conn, who, update)?
    };
    if changed {
        notify_changed();
    }
    Ok(())
}

pub(crate) fn apply_tx(conn: &Connection, who: &[u8; 32], update: &AvatarUpdate) -> Result<bool> {
    if let Some(bytes) = &update.avif {
        check_avif(bytes)?;
    }
    let revision = i64::try_from(update.revision)?;
    let changed = conn.execute(
        "INSERT INTO peer_avatars (ipk, avif, updated_at, revision) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(ipk) DO UPDATE SET avif = excluded.avif, updated_at = excluded.updated_at, \
             revision = excluded.revision WHERE excluded.revision > peer_avatars.revision",
        (who.as_slice(), update.avif.as_deref(), systime().as_secs(), revision),
    )?;
    Ok(changed != 0)
}

pub fn get(who: &[u8; 32]) -> Option<Vec<u8>> {
    let conn = MESSAGES_DB.lock();
    get_tx(&conn, who)
}

pub fn get_tx(conn: &Connection, who: &[u8; 32]) -> Option<Vec<u8>> {
    conn.query_row("SELECT avif FROM peer_avatars WHERE ipk = ?1", [who.as_slice()], |r| r.get(0))
        .ok()
        .flatten()
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
    fn reordered_updates_and_replays_cannot_resurrect_a_removed_picture() {
        let conn = open_in_memory();
        let who = [7u8; 32];
        let old = AvatarUpdate { revision: 10, avif: Some(avif_like(1)) };
        let latest = AvatarUpdate { revision: 12, avif: Some(avif_like(2)) };
        let removal = AvatarUpdate { revision: 13, avif: None };
        assert!(apply_tx(&conn, &who, &latest).unwrap());
        assert!(!apply_tx(&conn, &who, &old).unwrap());
        assert_eq!(get_tx(&conn, &who), latest.avif);
        assert!(apply_tx(&conn, &who, &removal).unwrap());
        // Duplicate copies arriving through another shared chat also do nothing.
        assert!(!apply_tx(&conn, &who, &latest).unwrap());
        assert!(!apply_tx(&conn, &who, &removal).unwrap());
        assert_eq!(get_tx(&conn, &who), None);
        let revision: u64 = conn.query_row(
            "SELECT revision FROM peer_avatars WHERE ipk = ?1", [who.as_slice()], |r| r.get(0),
        ).unwrap();
        assert_eq!(revision, removal.revision, "removal revision survives future reads");
        let next = AvatarUpdate { revision: 14, avif: Some(avif_like(3)) };
        assert!(apply_tx(&conn, &who, &next).unwrap());
        assert_eq!(get_tx(&conn, &who), next.avif);

        let fresh_peer = [8u8; 32];
        assert!(apply_tx(&conn, &fresh_peer, &removal).unwrap());
        assert!(!apply_tx(&conn, &fresh_peer, &old).unwrap());
        assert_eq!(get_tx(&conn, &fresh_peer), None, "removal may arrive before any upload");
    }

    #[test]
    fn the_gate_refuses_what_is_not_a_small_avif() {
        let conn = open_in_memory();
        let who = [8u8; 32];

        let mut oversized = avif_like(0);
        oversized.resize(MAX_AVATAR_BYTES + 1, 0);
        assert!(apply_tx(&conn, &who, &AvatarUpdate { revision: 1, avif: Some(oversized) }).is_err(), "over the cap");

        assert!(apply_tx(&conn, &who, &AvatarUpdate { revision: 1, avif: Some(b"not an image at all".to_vec()) }).is_err(), "no ftyp box");
        assert!(apply_tx(&conn, &who, &AvatarUpdate { revision: 1, avif: Some(vec![]) }).is_err(), "empty");

        assert_eq!(get_tx(&conn, &who), None, "a refused picture leaves no row");

        let mut at_cap = avif_like(0);
        at_cap.resize(MAX_AVATAR_BYTES, 0);
        apply_tx(&conn, &who, &AvatarUpdate { revision: 1, avif: Some(at_cap) }).expect("exactly the cap is allowed");
    }
}
