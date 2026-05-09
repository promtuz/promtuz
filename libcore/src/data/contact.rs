use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use rusqlite::params;

use crate::db::peers::CONTACTS_DB;
use crate::db::peers::ContactRow;

/// Promtuz "address book" entry — the long-term identity (`ipk`) plus the
/// nullable handle of the implicit 1:1 MLS group with this contact.
///
/// **Phase 4 of the MLS rollout** (`misc/specs/MLS.md` §11.3) replaced the
/// v2-era static-shared-key fields (`epk`, `enc_esk`) with `mls_group_id`.
/// On first send to a contact whose `mls_group_id` is `None`, the
/// messaging layer fetches their KeyPackage, builds a fresh group, and
/// persists the group id back via [`Self::set_mls_group_id`].
#[derive(Debug, Clone)]
pub struct Contact {
    pub inner: Arc<ContactRow>,
}

impl Contact {
    /// Save a new contact. The `mls_group_id` defaults to `None`; the
    /// messaging layer flips it to the freshly-minted group id on the
    /// first dispatch to this peer.
    pub fn save(ipk: [u8; 32], name: String) -> Result<Self> {
        let added_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        let conn = CONTACTS_DB.lock();
        conn.execute(
            "INSERT OR REPLACE INTO contacts (ipk, name, added_at, mls_group_id) \
             VALUES (?1, ?2, ?3, NULL)",
            params![ipk, name.clone(), added_at],
        )?;

        Ok(Self {
            inner: Arc::new(ContactRow { ipk, name, added_at, mls_group_id: None }),
        })
    }

    pub fn get(ipk: &[u8; 32]) -> Option<Self> {
        let conn = CONTACTS_DB.lock();
        conn.query_row(
            "SELECT * FROM contacts WHERE ipk = ?1",
            [ipk.as_slice()],
            ContactRow::from_row,
        )
        .ok()
        .map(|inner| Self { inner: Arc::new(inner) })
    }

    pub fn list() -> Vec<ContactRow> {
        let conn = CONTACTS_DB.lock();
        let mut stmt = conn
            .prepare("SELECT * FROM contacts ORDER BY added_at DESC")
            .expect("failed to prepare");
        stmt.query_map([], ContactRow::from_row)
            .expect("failed to query")
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn exists(ipk: &[u8; 32]) -> bool {
        let conn = CONTACTS_DB.lock();
        conn.query_row("SELECT 1 FROM contacts WHERE ipk = ?1", [ipk.as_slice()], |_| Ok(()))
            .is_ok()
    }

    /// Persist the MLS group id for this contact. Called by
    /// `messaging::send_message_inner` after lazy-creating the implicit
    /// 1:1 group on first send.
    ///
    /// Idempotent: re-binding the same group id is a no-op. Replacing an
    /// existing non-null id is allowed (e.g. if the user re-pairs after a
    /// session reset) — the caller is responsible for ensuring the prior
    /// group has been left/rejoined cleanly.
    pub fn set_mls_group_id(ipk: &[u8; 32], group_id: &[u8; 32]) -> Result<()> {
        let conn = CONTACTS_DB.lock();
        conn.execute(
            "UPDATE contacts SET mls_group_id = ?1 WHERE ipk = ?2",
            params![group_id, ipk],
        )?;
        Ok(())
    }
}
