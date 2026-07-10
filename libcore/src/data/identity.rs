use anyhow::Result;
use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use common::crypto::get_signing_key;
use common::crypto::sign::derive_p2p_tls_key;
use common::proto::mls_wire::Invite;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WELCOME_LIFETIME_MS;
use common::proto::mls_wire::invite_signing_input;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::platform::SECURE_STORE;
use crate::db::identity::IDENTITY_DB;
use crate::db::identity::IdentityRow;
use crate::utils::systime;

/// Pairing invites live ~10 minutes — long enough to cover async Welcome
/// delivery + a reconnect, short enough to bound a shoulder-surfed QR.
const INVITE_TTL_MS: u64 = 10 * 60 * 1000;

/// Opened identity secret, cached for the process lifetime so a StrongBox
/// `open()` (~1s on real hardware) runs once per launch instead of once per
/// signature. Keyed by the current IPK: an identity switch (recovery/restore)
/// changes the IPK, the key stops matching, and the next call re-opens — the
/// cache is self-busting, no explicit invalidation needed.
///
/// Holds one long-lived copy of the raw secret. Accepted exposure:
/// `IDENTITY_RECOVERY.md` §8 already treats the isk as a bearer secret held
/// raw in platform escrow and as a BIP39 phrase; a process-lifetime RAM copy
/// is strictly less exposed. `Zeroizing` clears it on eviction/replacement.
struct CachedIsk {
    ipk:    [u8; 32],
    secret: Zeroizing<[u8; 32]>,
}

static ISK_CACHE: Lazy<RwLock<Option<CachedIsk>>> = Lazy::new(|| RwLock::new(None));

/// Cache lookup + double-checked open. Pure w.r.t. its inputs so the
/// hit / miss / identity-switch logic is unit-testable without StrongBox or
/// the identity DB: `current_ipk` is read cheaply by the caller, `open`
/// performs the StrongBox decrypt only on a miss.
fn cached_or_open(
    cache: &RwLock<Option<CachedIsk>>,
    current_ipk: [u8; 32],
    open: impl FnOnce() -> Result<[u8; 32]>,
) -> Result<Zeroizing<SecretKey>> {
    if let Some(c) = cache.read().as_ref()
        && c.ipk == current_ipk
    {
        return Ok(Zeroizing::new(SecretKey::from(*c.secret)));
    }
    // Miss, or the identity changed. Take the write lock and re-check —
    // another thread may have opened while we waited on the lock.
    let mut guard = cache.write();
    if let Some(c) = guard.as_ref()
        && c.ipk == current_ipk
    {
        return Ok(Zeroizing::new(SecretKey::from(*c.secret)));
    }
    let secret = open()?;
    *guard = Some(CachedIsk { ipk: current_ipk, secret: Zeroizing::new(secret) });
    Ok(Zeroizing::new(SecretKey::from(secret)))
}

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

    /// Replace the display name (backup import restores the backed-up one
    /// over the restore-flow placeholder).
    pub fn set_name(name: &str) -> Result<()> {
        let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
        let conn = IDENTITY_DB.lock();
        conn.execute("UPDATE identity SET name = ?1 WHERE id = 0", [name])?;
        Ok(())
    }

    /// Restore a previously-created identity from its raw secret — the shared
    /// tail of both recovery channels (escrow bytes or decoded BIP39 phrase).
    /// Refuses when an identity already exists: restore is a fresh-install
    /// flow and must never clobber a live identity.
    pub(super) fn restore(isk: &[u8; 32], name: &str) -> Result<()> {
        if Identity::get().is_some() {
            return Err(anyhow!("an identity already exists; restore requires a fresh install"));
        }
        let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
        let store = SECURE_STORE.get().ok_or(anyhow!("API is not initialized"))?;

        let ipk = SigningKey::from_bytes(isk).verifying_key();
        let enc_isk = store.seal(isk.to_vec()).map_err(|e| anyhow!("seal failed: {e}"))?;

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
        // The ~10-min `expiry_ms` is scan-time freshness UX (surfaced by
        // `preview_invite`); the ACCEPT gate must tolerate the delivery
        // channel's latency — a pairing Welcome can legitimately sit in
        // the home stash up to WELCOME_LIFETIME_MS before we reconnect
        // and fetch it. Rejecting here on the scan window permanently
        // killed pairings whose Welcome arrived after 10 minutes.
        if systime().as_millis() as u64
            >= invite.expiry_ms.saturating_add(WELCOME_LIFETIME_MS)
        {
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
        // Cheap sqlite read (no StrongBox) — the cache key. A recovery/restore
        // that swaps the identity changes this and self-busts the cache.
        let current_ipk = Identity::public_key()?.to_bytes();
        cached_or_open(&ISK_CACHE, current_ipk, || {
            let store = SECURE_STORE.get().ok_or(anyhow!("API is not initialized"))?;
            let conn = IDENTITY_DB.lock();
            conn.query_one("SELECT enc_isk FROM identity WHERE id = 0", [], |row| {
                let eisk: Vec<u8> = row.get("enc_isk")?;
                let secret = store.open(eisk).map_err(|_| rusqlite::Error::UnwindingPanic)?;
                let secret: [u8; 32] =
                    secret.try_into().map_err(|_| rusqlite::Error::UnwindingPanic)?;
                Ok(secret)
            })
            .map_err(Into::into)
        })
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

#[cfg(test)]
mod cache_tests {
    use super::{CachedIsk, cached_or_open};
    use parking_lot::RwLock;
    use std::cell::Cell;

    #[test]
    fn opens_once_then_serves_from_cache() {
        let cache: RwLock<Option<CachedIsk>> = RwLock::new(None);
        let ipk = [1u8; 32];
        let opens = Cell::new(0);

        let a = cached_or_open(&cache, ipk, || {
            opens.set(opens.get() + 1);
            Ok([9u8; 32])
        })
        .unwrap();
        let b = cached_or_open(&cache, ipk, || {
            opens.set(opens.get() + 1);
            Ok([9u8; 32])
        })
        .unwrap();

        assert_eq!(opens.get(), 1, "second call must hit the cache, not re-open");
        assert_eq!(&a[..], &b[..]);
    }

    #[test]
    fn ipk_switch_busts_cache() {
        let cache: RwLock<Option<CachedIsk>> = RwLock::new(None);
        let opens = Cell::new(0);

        let _a = cached_or_open(&cache, [1u8; 32], || {
            opens.set(opens.get() + 1);
            Ok([9u8; 32])
        })
        .unwrap();
        let b = cached_or_open(&cache, [2u8; 32], || {
            opens.set(opens.get() + 1);
            Ok([7u8; 32])
        })
        .unwrap();

        assert_eq!(opens.get(), 2, "identity switch must re-open");
        assert_eq!(&b[..], &[7u8; 32][..], "must serve the new identity's secret");
    }
}
