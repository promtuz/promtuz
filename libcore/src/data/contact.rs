use std::sync::Arc;

use anyhow::Result;
use common::utils::now_secs;
use rusqlite::Connection;
use rusqlite::params;

use crate::db::all;
use crate::db::peers::ContactRow;
use crate::state::core;

/// Pairing status, stored in `contacts.status`.
pub const PAIR_STATUS_PENDING: u8 = 0;
pub const PAIR_STATUS_PAIRED: u8 = 1;
pub const PAIR_STATUS_REJECTED: u8 = 2;
/// Someone we never added messaged us, and we have not accepted.
pub const PAIR_STATUS_REQUEST: u8 = 3;

#[derive(Debug, Clone)]
pub struct Contact {
    pub inner: Arc<ContactRow>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveOutcome {
    Created,
    Existed,
}

impl Contact {
    /// A re-pair keeps `added_at` and `mls_group_id`, so the established 1:1 group is not forked.
    pub fn save(ipk: [u8; 32], name: String) -> Result<SaveOutcome> {
        // Self is never a contact: a 1:1 group with yourself fails on CannotDecryptOwnMessage once
        // the relay reflects your own dispatch back.
        if crate::data::identity::Identity::public_key().is_ok_and(|k| k.to_bytes() == ipk) {
            return Err(anyhow::anyhow!("cannot add yourself as a contact"));
        }
        let added_at = now_secs();

        let conn = core().db.contacts().lock();
        let existed = conn
            .query_row("SELECT 1 FROM contacts WHERE ipk = ?1", [ipk.as_slice()], |_| Ok(()))
            .is_ok();

        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id) \
             VALUES (?1, ?2, ?3, NULL) \
             ON CONFLICT(ipk) DO UPDATE SET name = excluded.name, \
             status = CASE WHEN status = ?4 THEN ?5 ELSE status END",
            params![ipk, name, added_at, PAIR_STATUS_REQUEST, PAIR_STATUS_PAIRED],
        )?;

        // The home list reads conversations. A failure here costs that entry, not the contact.
        drop(conn);
        if let Err(e) = crate::data::conversation::Conversation::for_peer(&ipk) {
            log::warn!("CONTACT: could not open a conversation for a new contact: {e}");
        }

        crate::profile_sync::store::wake();
        Ok(if existed { SaveOutcome::Existed } else { SaveOutcome::Created })
    }

    pub fn get(ipk: &[u8; 32]) -> Option<Self> {
        let conn = core().db.contacts().lock();
        conn.query_row(
            "SELECT * FROM contacts WHERE ipk = ?1",
            [ipk.as_slice()],
            ContactRow::from_row,
        )
        .ok()
        .map(|inner| Self { inner: Arc::new(inner) })
    }

    pub fn list() -> Vec<ContactRow> {
        Self::dump_all_tx(&core().db.contacts().lock()).unwrap_or_default()
    }

    pub(crate) fn dump_all_tx(conn: &Connection) -> rusqlite::Result<Vec<ContactRow>> {
        all(conn, "SELECT * FROM contacts ORDER BY added_at DESC", [], ContactRow::from_row)
    }

    /// A restored `mls_group_id` may have no local state; the send and receive paths recreate it.
    pub fn import_rows_tx(conn: &Connection, rows: &[ContactRow], replace: bool) -> Result<usize> {
        let sql = format!(
            "INSERT OR {} INTO contacts (ipk, name, added_at, mls_group_id, status, reject_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            if replace { "REPLACE" } else { "IGNORE" },
        );
        let mut n = 0usize;
        for r in rows {
            n += conn.execute(
                &sql,
                params![r.ipk, r.name, r.added_at, r.mls_group_id, r.status, r.reject_reason],
            )?;
        }
        Ok(n)
    }

    pub fn exists(ipk: &[u8; 32]) -> bool {
        let conn = core().db.contacts().lock();
        conn.query_row("SELECT 1 FROM contacts WHERE ipk = ?1", [ipk.as_slice()], |_| Ok(()))
            .is_ok()
    }

    pub fn set_mls_group_id(ipk: &[u8; 32], group_id: &[u8; 32]) -> Result<()> {
        let conn = core().db.contacts().lock();
        conn.execute(
            "UPDATE contacts SET mls_group_id = ?1 WHERE ipk = ?2",
            params![group_id, ipk],
        )?;
        Ok(())
    }

    /// The inviter's save before the invitee proves the pair. A re-pair never downgrades PAIRED.
    pub fn save_pending(ipk: [u8; 32], name: String) -> Result<()> {
        if crate::data::identity::Identity::public_key().is_ok_and(|k| k.to_bytes() == ipk) {
            return Err(anyhow::anyhow!("cannot add yourself as a contact"));
        }
        Self::save_pending_tx(&core().db.contacts().lock(), ipk, &name, now_secs())?;
        if let Err(e) = crate::data::conversation::Conversation::for_peer(&ipk) {
            log::warn!("CONTACT: could not open a conversation for a pending contact: {e}");
        }
        Ok(())
    }

    pub(crate) fn save_pending_tx(
        conn: &Connection, ipk: [u8; 32], name: &str, added_at: u64,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id, status) \
             VALUES (?1, ?2, ?3, NULL, ?4) \
             ON CONFLICT(ipk) DO UPDATE SET name = excluded.name, \
             status = CASE WHEN status = ?5 THEN ?4 ELSE status END",
            params![ipk, name, added_at, PAIR_STATUS_PENDING, PAIR_STATUS_REQUEST],
        )?;
        Ok(())
    }

    /// Nothing we send reaches them until [`Self::accept_request`].
    pub fn save_request(ipk: [u8; 32]) -> Result<()> {
        let conn = core().db.contacts().lock();
        conn.execute(
            "INSERT OR IGNORE INTO contacts (ipk, name, added_at, mls_group_id, status) \
             VALUES (?1, '', ?2, NULL, ?3)",
            params![ipk, now_secs(), PAIR_STATUS_REQUEST],
        )?;
        drop(conn);
        crate::data::conversation::Conversation::for_peer(&ipk)?;
        Ok(())
    }

    pub fn accept_request(ipk: &[u8; 32]) -> Result<bool> {
        let conn = core().db.contacts().lock();
        let changed = conn.execute(
            "UPDATE contacts SET status = ?1 WHERE ipk = ?2 AND status = ?3",
            params![PAIR_STATUS_PAIRED, ipk, PAIR_STATUS_REQUEST],
        )? == 1;
        crate::profile_sync::store::wake();
        Ok(changed)
    }

    /// They deleted our pair. The contact stays, pending the fresh pair our next
    /// message starts; until they accept it, they are no longer confirmed.
    pub fn unpair(ipk: &[u8; 32]) -> Result<bool> {
        let conn = core().db.contacts().lock();
        let changed = conn.execute(
            "UPDATE contacts SET mls_group_id = NULL, status = ?1 WHERE ipk = ?2 AND status IN (?1, ?3)",
            params![PAIR_STATUS_PENDING, ipk, PAIR_STATUS_PAIRED],
        )? == 1;
        crate::profile_sync::store::wake();
        Ok(changed)
    }

    pub fn count_requests() -> u32 {
        let conn = core().db.contacts().lock();
        conn.query_row("SELECT COUNT(*) FROM contacts WHERE status = ?1", [PAIR_STATUS_REQUEST], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn mark_paired(ipk: &[u8; 32]) {
        let _ = Self::mark_paired_tx(&core().db.contacts().lock(), ipk);
        crate::profile_sync::store::wake();
    }

    pub(crate) fn mark_paired_tx(conn: &Connection, ipk: &[u8; 32]) -> rusqlite::Result<usize> {
        conn.execute(
            "UPDATE contacts SET status = ?1 WHERE ipk = ?2 AND status = ?3",
            params![PAIR_STATUS_PAIRED, ipk, PAIR_STATUS_PENDING],
        )
    }

    /// Gated on `mls_group_id IS NULL`: a live group is a working pair, and a stray decline for a
    /// late handshake must never tear it down.
    pub fn mark_rejected(ipk: &[u8; 32], reason: u8) {
        let conn = core().db.contacts().lock();
        let _ = conn.execute(
            "UPDATE contacts SET status = ?1, reject_reason = ?2 WHERE ipk = ?3 AND mls_group_id IS NULL",
            params![PAIR_STATUS_REJECTED, reason, ipk],
        );
    }

    pub fn status(ipk: &[u8; 32]) -> Option<u8> {
        Self::status_tx(&core().db.contacts().lock(), ipk)
    }

    pub(crate) fn status_tx(conn: &Connection, ipk: &[u8; 32]) -> Option<u8> {
        conn.query_row(
            "SELECT status FROM contacts WHERE ipk = ?1",
            [ipk.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .ok()
        .map(|s| s as u8)
    }

    pub fn is_paired(ipk: &[u8; 32]) -> bool {
        Self::is_paired_tx(&core().db.contacts().lock(), ipk)
    }

    pub(crate) fn is_paired_tx(conn: &Connection, ipk: &[u8; 32]) -> bool {
        Self::status_tx(conn, ipk) == Some(PAIR_STATUS_PAIRED)
    }

    /// Last step of forgetting a contact, once its `mls_group_id` has been consumed.
    pub fn delete(ipk: &[u8; 32]) -> Result<()> {
        let conn = core().db.contacts().lock();
        conn.execute("DELETE FROM contacts WHERE ipk = ?1", params![ipk])?;
        crate::profile_sync::store::wake();
        Ok(())
    }
}
