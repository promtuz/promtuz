use anyhow::Result;
use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use common::crypto::sign::derive_p2p_tls_key;
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

    /// Decrypts and returns the identity secret key wrapped in `Zeroizing`.
    ///
    /// Visibility is intentionally `pub(super)` (module-private to `data`):
    /// callers outside the data layer must go through [`IdentitySigner`] so
    /// raw key material never leaves this module. Returning the bare
    /// `[u8; 32]` (the previous `secret_key_bytes`) defeated the
    /// `Zeroizing` wrapper and let the secret persist on caller stacks.
    pub(super) fn secret_key_with_manager() -> Result<Zeroizing<SecretKey>> {
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

    /// Derive the per-identity Ed25519 sub-key dedicated to TLS-layer signing
    /// in peer-to-peer QUIC handshakes.
    ///
    /// The derivation is HKDF-SHA256 over the long-term identity secret with
    /// the IPK as salt — deterministic, so callers may safely re-derive the
    /// same key (the cert SPKI binds the connection to it). See
    /// [`common::crypto::sign::derive_p2p_tls_key`] for the rationale; in
    /// short, we never want one Ed25519 key to be the signer for both the
    /// rustls TLS 1.3 transcript *and* application-layer messages
    /// (`DispatchP`, IPK<->TLS sub-key bindings, …).
    ///
    /// `SigningKey` self-zeroizes on drop (the workspace enables the
    /// `zeroize` feature on `ed25519-dalek`); callers should still hold it
    /// behind an `Arc` for the lifetime of a `rustls::sign::SigningKey`
    /// (one per peer connection) rather than re-deriving per signature.
    pub fn tls_subkey() -> Result<SigningKey> {
        let secret = Identity::secret_key_with_manager()?;
        let public = SigningKey::from_bytes(&secret).verifying_key();
        Ok(derive_p2p_tls_key(&secret, public.as_bytes()))
    }

    /// Sign a message with the long-term identity key, returning both the
    /// signature and the long-term IPK pubkey.
    ///
    /// Used by the peer-to-peer identity-exchange flow to bind the TLS
    /// sub-key (carried as the cert SPKI) back to the user's true IPK: the
    /// scanner/sharer signs the peer's TLS sub-key pubkey with their IPK,
    /// the receiver verifies, and only then is the contact saved against
    /// the IPK rather than the TLS sub-key.
    pub fn sign_with_ipk(message: &[u8]) -> Result<(Signature, [u8; 32])> {
        let secret = Identity::secret_key_with_manager()?;
        let key = SigningKey::from_bytes(&secret);
        let pub_bytes = key.verifying_key().to_bytes();
        Ok((key.sign(message), pub_bytes))
    }
}

/// **Phase 4 helper** for the MLS messaging path: hand the caller a
/// `SigningKey` clone of the long-term IPK secret. The MLS layer needs
/// to perform multiple signing operations across an async send (the
/// outer envelope sig + welcome envelope sig + KP record sigs) and
/// each call to `IdentitySigner::sign` re-decrypts via the Keystore
/// — too costly. This helper decrypts once and returns the
/// `SigningKey`; it is the *only* path outside the data layer that
/// holds a bare `SigningKey`, and the caller is expected to drop it
/// promptly (the workspace `zeroize` feature on `ed25519-dalek` clears
/// the bytes on drop).
///
/// `expected_ipk` is checked against the verifying half so a caller
/// that has stale identity state can't accidentally sign with a
/// different secret.
pub(crate) fn secret_key_signing(expected_ipk: &[u8; 32]) -> Result<SigningKey> {
    let secret = Identity::secret_key_with_manager()?;
    let key = SigningKey::from_bytes(&secret);
    if &key.verifying_key().to_bytes() != expected_ipk {
        return Err(anyhow!("identity secret does not match expected IPK"));
    }
    Ok(key)
}
