use std::sync::Arc;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use common::crypto::StaticSecret;
use common::crypto::get_shared_key;

use crate::KEY_MANAGER;
use crate::db::peers::CONTACTS_DB;
use crate::db::peers::ContactRow;

#[derive(Debug, Clone)]
pub struct Contact {
    pub inner: Arc<ContactRow>,
}

impl Contact {
    /// Save a contact with their EPK and our encrypted ephemeral secret key.
    pub fn save(ipk: [u8; 32], epk: [u8; 32], enc_esk: Vec<u8>, name: String) -> Result<Self> {
        let added_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

        let conn = CONTACTS_DB.lock();
        conn.execute(
            "INSERT OR REPLACE INTO contacts (ipk, epk, enc_esk, name, added_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            (ipk, epk, enc_esk.clone(), name.clone(), added_at),
        )?;

        Ok(Self { inner: Arc::new(ContactRow { ipk, epk, enc_esk, name, added_at }) })
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

    /// Derive the shared symmetric key for this friendship.
    /// Decrypts our stored ephemeral secret, does DH with their EPK.
    pub fn shared_key(&self) -> Result<[u8; 32]> {
        let km = KEY_MANAGER.get().unwrap();

        let esk_bytes = km
            .decrypt(&self.inner.enc_esk)
            .map_err(|e| anyhow::anyhow!("failed to decrypt esk: {e:?}"))?;
        let esk_arr: [u8; 32] =
            esk_bytes.try_into().map_err(|_| anyhow::anyhow!("esk wrong length"))?;

        let our_secret = StaticSecret::from(esk_arr);
        let their_pk = common::crypto::xPublicKey::from(self.inner.epk);
        let shared = our_secret.diffie_hellman(&their_pk);

        Ok(get_shared_key(shared.as_bytes(), b"promtuz-msg-v1", ""))
    }
}
