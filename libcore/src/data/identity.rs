use anyhow::Result;
use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use common::crypto::get_signing_key;
use common::crypto::sign::derive_p2p_tls_key;
use common::proto::mls_wire::Invite;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::invite_signing_input;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::platform::SECURE_STORE;
use crate::db::identity::IDENTITY_DB;
use crate::db::identity::IdentityRow;
use crate::utils::systime;

/// Pairing invites live ~10 minutes — long enough to cover async Welcome
/// delivery + a reconnect, short enough to bound a shoulder-surfed QR.
const INVITE_TTL_MS: u64 = 10 * 60 * 1000;

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

    /// Create + persist a fresh identity: validate the nickname, generate
    /// the long-term Ed25519 secret, seal it via the platform key store,
    /// and store the row. Entry point for `api::identity::enroll`.
    pub fn create(name: &str) -> Result<()> {
        let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
        let store = SECURE_STORE.get().ok_or(anyhow!("API is not initialized"))?;

        let isk = get_signing_key();
        let ipk = isk.verifying_key();
        let enc_isk =
            store.seal(isk.as_bytes().to_vec()).map_err(|e| anyhow!("seal failed: {e}"))?;

        Identity::save(IdentityRow {
            id: 0,
            ipk: ipk.to_bytes(),
            enc_isk,
            created_at: systime().as_millis() as u64,
            name,
        })?;
        Ok(())
    }

    /// Mint a bearer pairing invite valid for ~10 minutes. Signed by our
    /// long-term IPK; whoever holds it may add us until it expires. Used
    /// by `api::identity::make_invite_qr`.
    pub fn mint_invite() -> Result<Invite> {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;

        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let expiry_ms = systime().as_millis() as u64 + INVITE_TTL_MS;

        let msg = invite_signing_input(MLS_WIRE_VERSION, &id, expiry_ms);
        let sig = IdentitySigner::sign(&msg)?;

        Ok(Invite { id: id.into(), expiry_ms, sig: sig.to_bytes().into() })
    }

    /// Verify an inbound invite was minted by *us* (signed under our IPK,
    /// unexpired). This is the whole anti-spam gate — no server is trusted.
    /// Used by the welcome gate in `messaging`.
    pub fn verify_invite(invite: &Invite) -> bool {
        let Some(our_ipk) = Identity::get().map(|i| i.ipk()) else {
            return false;
        };
        if systime().as_millis() as u64 >= invite.expiry_ms {
            return false;
        }
        let Ok(vk) = VerifyingKey::from_bytes(&our_ipk) else {
            return false;
        };
        let msg = invite_signing_input(MLS_WIRE_VERSION, &invite.id.0, invite.expiry_ms);
        vk.verify_strict(&msg, &Signature::from_bytes(&invite.sig.0)).is_ok()
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
        let store = SECURE_STORE.get().ok_or(anyhow!("API is not initialized"))?;
        let conn = IDENTITY_DB.lock();

        Ok(conn.query_one("SELECT enc_isk FROM identity WHERE id = 0", [], |row| {
            let eisk: Vec<u8> = row.get("enc_isk")?;
            let secret = store.open(eisk).map_err(|_| rusqlite::Error::UnwindingPanic)?;
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

/// Helper for the MLS messaging path: hand the caller a `SigningKey`
/// clone of the long-term IPK secret. The MLS layer needs
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

/// Normalize + validate a user-chosen nickname (NFC, trimmed, ≤32 chars,
/// no control/zero-width characters). Returns the cleaned name or a
/// user-facing error message.
fn validate_nickname(name: &str) -> std::result::Result<String, String> {
    let normalized: String = name.nfc().collect();
    let trimmed = normalized.trim();

    if trimmed.is_empty() {
        return Err("Nickname cannot be empty".into());
    }
    if trimmed.chars().count() > 32 {
        return Err("Nickname too long (max 32 characters)".into());
    }
    if trimmed.chars().any(|c| c.is_control() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{FEFF}')) {
        return Err("Nickname contains invalid characters".into());
    }

    Ok(trimmed.to_string())
}
