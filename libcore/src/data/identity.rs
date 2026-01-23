use anyhow::Result;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use jni::JNIEnv;
use zeroize::Zeroizing;

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

    pub fn secret_key(env: &mut JNIEnv) -> Result<Zeroizing<SecretKey>> {
        Self::secret_key_with_manager(&KeyManager::new(env)?)
    }

    fn secret_key_with_manager(key_manager: &KeyManager) -> Result<Zeroizing<SecretKey>> {
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

pub struct IdentitySigner {
    // caching the keymanager, due to JNI lifetime shenanigans
    key_manager: KeyManager,
}

impl IdentitySigner {
    pub fn new(env: &mut JNIEnv) -> Result<IdentitySigner> {
        let key_manager = KeyManager::new(env)?;
        Ok(Self { key_manager })
    }

    // both secret and key should be zeroized
    pub fn sign(&self, message: &[u8]) -> Result<Signature> {
        let secret = Identity::secret_key_with_manager(&self.key_manager)?;
        let key = SigningKey::from_bytes(&secret);
        Ok(key.sign(message))
    }
}
