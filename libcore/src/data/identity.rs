use anyhow::Result;
use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use zeroize::Zeroizing;

use crate::KEY_MANAGER;
use crate::db::identity::IDENTITY_DB;
use crate::db::identity::IdentityRow;

pub struct Identity {
    inner: IdentityRow,
}

impl Identity {
    pub fn ipk(&self) -> [u8; 32] {
        self.inner.ipk
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
                    id, ipk, enc_isk, created_at, name
                 ) VALUES (?1, ?2, ?3, ?4, ?5);",
            (
                identity.id,
                identity.ipk,
                identity.enc_isk.clone(),
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
            row.get("ipk")
                .map(|k: [u8; 32]| PublicKey::from_bytes(&k).expect("not a ed25519 public key"))
        })
    }

    pub fn secret_key() -> Result<Zeroizing<SecretKey>> {
        Self::secret_key_with_manager()
    }

    /// Get secret key bytes without requiring a JNIEnv parameter.
    /// Attaches to JVM internally. Safe to call from async contexts.
    pub fn secret_key_bytes() -> [u8; 32] {
        let sk = Self::secret_key_with_manager().unwrap();
        *sk
    }

    fn secret_key_with_manager() -> Result<Zeroizing<SecretKey>> {
        let key_manager = KEY_MANAGER.get().ok_or(anyhow!("API is not initialized"))?;
        let conn = IDENTITY_DB.lock();

        Ok(conn.query_one("SELECT enc_isk FROM identity WHERE id = 0", [], |row| {
            let eisk: Vec<u8> = row.get("enc_isk")?;
            let secret = key_manager.decrypt(&eisk).map_err(|_| rusqlite::Error::UnwindingPanic)?;
            let secret: [u8; 32] =
                secret.try_into().map_err(|_| rusqlite::Error::UnwindingPanic)?;

            Ok(Zeroizing::new(SecretKey::from(secret)))
        })?)
    }
}

#[derive(Debug)]
pub struct IdentitySigner;

impl IdentitySigner {
    /// Signs message using the identity key.
    /// The secret key is decrypted on-demand and immediately dropped.
    pub fn sign(message: &[u8]) -> Result<Signature> {
        let secret = Identity::secret_key_with_manager()?;
        let key = SigningKey::from_bytes(&secret);
        Ok(key.sign(message))
    }
}
