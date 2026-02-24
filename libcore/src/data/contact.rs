use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::db::peers::{CONTACTS_DB, ContactRow};

pub struct Contact {
    pub inner: ContactRow,
}

impl Contact {
    pub fn save(ipk: [u8; 32], epk: [u8; 32], name: String) -> Result<Self> {
        let added_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let conn = CONTACTS_DB.lock();
        conn.execute(
            "INSERT OR REPLACE INTO contacts (ipk, epk, name, added_at) VALUES (?1, ?2, ?3, ?4)",
            (ipk, epk, name.clone(), added_at),
        )?;

        Ok(Self {
            inner: ContactRow { ipk, epk, name, added_at },
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
        .map(|inner| Self { inner })
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
        conn.query_row(
            "SELECT 1 FROM contacts WHERE ipk = ?1",
            [ipk.as_slice()],
            |_| Ok(()),
        )
        .is_ok()
    }
}
