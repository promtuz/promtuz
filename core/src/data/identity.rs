use anyhow::Result;
use common::crypto::PublicKey;
use common::crypto::StaticSecret;
use jni::JNIEnv;

use crate::db::identity::IDENTITY_DB;
use crate::db::identity::IdentityRow;
use crate::ndk::key_manager::KeyManager;

pub struct Identity {
    inner: IdentityRow,
}

impl Identity {
    pub fn ipk(&self) -> [u8; 32] {
        self.inner.ipk
    }
    pub fn vfk(&self) -> [u8; 32] {
        self.inner.vfk
    }
    pub fn name(&self) -> String {
        self.inner.name.clone()
    }

    pub fn get() -> Option<Self> {
        let conn = IDENTITY_DB.lock();
        conn.query_row("SELECT * FROM identity WHERE id = 0", [], IdentityRow::from_row)
            .ok()
            .map(|ir| Self { inner: ir })
    }

    pub fn save(identity: IdentityRow) -> rusqlite::Result<Self> {
        let conn = IDENTITY_DB.lock();

        conn.execute(
            "INSERT INTO identity (
                    id, ipk, enc_isk, vfk, enc_vsk, created_at, name
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7);",
            (
                0,
                identity.ipk,
                identity.enc_isk.clone(),
                identity.vfk,
                identity.enc_vsk.clone(),
                identity.created_at,
                identity.name.clone(),
            ),
        )?;

        Ok(Identity { inner: identity })
    }

    /// Fetches identity public key
    pub fn public_key() -> rusqlite::Result<PublicKey> {
        let conn = IDENTITY_DB.lock();
        conn.query_one("SELECT ipk FROM identity WHERE id = 0", [], |row| {
            row.get("ipk").map(|k: [u8; 32]| PublicKey::from(k))
        })
    }

    pub fn secret_key(env: &mut JNIEnv) -> Result<StaticSecret> {
        let key_manager = KeyManager::new(env)?;
        let conn = IDENTITY_DB.lock();

        Ok(conn.query_one("SELECT enc_isk FROM identity WHERE id = 0", [], |row| {
            let eisk: Vec<u8> = row.get("enc_isk")?;
            let secret = key_manager.decrypt(&eisk).map_err(|_| rusqlite::Error::UnwindingPanic)?;
            let secret: [u8; 32] =
                secret.try_into().map_err(|_| rusqlite::Error::UnwindingPanic)?;

            Ok(StaticSecret::from(secret))
        })?)
    }
}
