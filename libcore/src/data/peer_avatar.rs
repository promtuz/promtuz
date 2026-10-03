//! Pictures people assert about themselves, stored as the AVIF bytes they sent.

use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::bail;
use common::proto::mls_wire::MAX_AVATAR_BYTES;
use common::utils::now_secs;
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

/// Apply only a newer owner-issued revision, regardless of delivery order or
/// which shared chat carried it. A NULL picture is a durable removal tombstone.
pub fn apply(who: &[u8; 32], update: &AvatarUpdate) -> Result<()> {
    let changed = {
        let conn = core().db.messages().lock();
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
        (who.as_slice(), update.avif.as_deref(), now_secs(), revision),
    )?;
    Ok(changed != 0)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::data::open;

    /// The smallest bytes the gate accepts: an ISOBMFF `ftyp` box. The store never decodes them.
    fn avif_like(fill: u8) -> Vec<u8> {
        let mut v = b"\0\0\0\x1cftypavif".to_vec();
        v.push(fill);
        v
    }

    #[test]
    fn stale_copies_and_non_avif_bytes_never_replace_a_picture() {
        let conn = open(crate::db::messages::migrate);
        let who = [7u8; 32];
        let old = AvatarUpdate { revision: 10, avif: Some(avif_like(1)) };
        let latest = AvatarUpdate { revision: 12, avif: Some(avif_like(2)) };
        let removal = AvatarUpdate { revision: 13, avif: None };
        assert!(apply_tx(&conn, &who, &latest).unwrap());
        assert!(!apply_tx(&conn, &who, &old).unwrap());
        assert_eq!(get_tx(&conn, &who), latest.avif);
        assert!(apply_tx(&conn, &who, &removal).unwrap());
        // Copies of the same update arriving through other shared chats change nothing.
        assert!(!apply_tx(&conn, &who, &latest).unwrap());
        assert!(!apply_tx(&conn, &who, &removal).unwrap());
        assert_eq!(get_tx(&conn, &who), None, "the removal holds");
        let next = AvatarUpdate { revision: 14, avif: Some(avif_like(3)) };
        assert!(apply_tx(&conn, &who, &next).unwrap());
        assert_eq!(get_tx(&conn, &who), next.avif);

        let fresh = [8u8; 32];
        assert!(apply_tx(&conn, &fresh, &removal).unwrap());
        assert!(!apply_tx(&conn, &fresh, &old).unwrap(), "a removal may arrive before any upload");
        assert_eq!(get_tx(&conn, &fresh), None);

        let mut at_cap = avif_like(0);
        at_cap.resize(MAX_AVATAR_BYTES, 0);
        let mut over_cap = at_cap.clone();
        over_cap.push(0);
        let stranger = [9u8; 32];
        for refused in [over_cap, b"not an image at all".to_vec(), Vec::new()] {
            let update = AvatarUpdate { revision: 1, avif: Some(refused) };
            assert!(apply_tx(&conn, &stranger, &update).is_err());
        }
        let rows: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM peer_avatars WHERE ipk = ?1",
                [stranger.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "a refused picture leaves no row, not even a revision");
        assert!(
            apply_tx(&conn, &stranger, &AvatarUpdate { revision: 1, avif: Some(at_cap) }).unwrap()
        );
    }
}
