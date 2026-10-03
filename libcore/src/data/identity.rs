use anyhow::Result;
use anyhow::anyhow;
use common::crypto::PublicKey;
use common::crypto::SecretKey;
use common::crypto::get_signing_key;
use common::crypto::sign::derive_p2p_tls_key;
use common::crypto::verify_ed25519;
use common::proto::mls_wire::Invite;
use common::proto::mls_wire::MLS_WIRE_VERSION;
use common::proto::mls_wire::WELCOME_LIFETIME_MS;
use common::proto::mls_wire::invite_signing_input;
use common::utils::now_ms;
use ed25519_dalek::Signature;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use std::sync::LazyLock;
use parking_lot::RwLock;
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::db::identity::IdentityRow;
use crate::state::core;

/// Covers async Welcome delivery and a reconnect while bounding a shoulder-surfed QR.
const INVITE_TTL_MS: u64 = 10 * 60 * 1000;

/// Keyed by IPK, so a restore misses; the ~1s StrongBox open runs once per launch. Accepted
/// exposure: the isk already exists raw in platform escrow and as a phrase.
struct CachedIsk {
    ipk:    [u8; 32],
    secret: Zeroizing<[u8; 32]>,
}

static ISK_CACHE: LazyLock<RwLock<Option<CachedIsk>>> = LazyLock::new(|| RwLock::new(None));

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
        let conn = core().db.identity().lock();
        conn.query_row("SELECT * FROM identity WHERE id = 0", [], IdentityRow::from_row)
            .ok()
            .map(|ir| Self { inner: ir })
    }

    pub fn local_ipk() -> Option<[u8; 32]> {
        Self::local_ipk_tx(&core().db.identity().lock())
    }

    pub(crate) fn local_ipk_tx(conn: &rusqlite::Connection) -> Option<[u8; 32]> {
        conn.query_row("SELECT ipk FROM identity WHERE id = 0", [], |r| r.get(0)).ok()
    }

    pub fn save(identity: IdentityRow) -> rusqlite::Result<Self> {
        let conn = core().db.identity().lock();

        conn.execute(
            "INSERT INTO identity (
                    id, ipk, enc_isk, created_at, name, avatar, avatar_revision, bio, profile_revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9);",
            (
                identity.id,
                identity.ipk,
                identity.enc_isk.clone(),
                identity.created_at,
                identity.name.clone(),
                identity.avatar.clone(),
                identity.avatar_revision,
                identity.bio.clone(),
                identity.profile_revision,
            ),
        )?;

        Ok(Identity { inner: identity })
    }

    pub fn create(name: &str) -> Result<()> {
        let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
        let store = core().secure_store.get().ok_or(anyhow!("API is not initialized"))?;

        let isk = get_signing_key();
        let ipk = isk.verifying_key();
        let enc_isk =
            store.seal(isk.as_bytes().to_vec()).map_err(|e| anyhow!("seal failed: {e}"))?;

        Identity::save(IdentityRow {
            id: 0,
            ipk: ipk.to_bytes(),
            enc_isk,
            created_at: now_ms(),
            name,
            avatar: None,
            avatar_revision: 0, bio: String::new(), profile_revision: 0,
        })?;
        Ok(())
    }

    pub fn details(&self) -> crate::data::peer_profile::ProfileUpdate {
        crate::data::peer_profile::ProfileUpdate {
            revision: self.inner.profile_revision, name: self.name(), bio: self.inner.bio.clone(),
            card: secret_key_signing(&self.ipk()).and_then(|key| crate::contact_card::make_card(&key, self.name())).unwrap_or_default(),
        }
    }

    pub fn set_details(name: &str, bio: &str) -> Result<()> {
        set_details_tx(&core().db.identity().lock(), name, bio, now_ms())?;
        crate::data::peer_avatar::notify_changed();
        Ok(())
    }

    pub fn avatar(&self) -> Option<Vec<u8>> {
        self.inner.avatar.clone()
    }

    pub fn avatar_update(&self) -> crate::data::peer_avatar::AvatarUpdate {
        crate::data::peer_avatar::AvatarUpdate {
            revision: self.inner.avatar_revision,
            avif: self.avatar(),
        }
    }

    /// Allocates the revision under the same lock; the caller broadcasts exactly this revision.
    pub fn set_avatar(avif: Option<&[u8]>) -> Result<u64> {
        let revision = {
            let conn = core().db.identity().lock();
            set_avatar_tx(&conn, avif, now_ms())?
        };
        crate::data::peer_avatar::notify_changed();
        Ok(revision)
    }

    pub(super) fn restore(isk: &[u8; 32], name: &str) -> Result<()> {
        if Identity::get().is_some() {
            return Err(anyhow!("an identity already exists; restore requires a fresh install"));
        }
        let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
        let store = core().secure_store.get().ok_or(anyhow!("API is not initialized"))?;

        let ipk = SigningKey::from_bytes(isk).verifying_key();
        let enc_isk = store.seal(isk.to_vec()).map_err(|e| anyhow!("seal failed: {e}"))?;

        Identity::save(IdentityRow {
            id: 0,
            ipk: ipk.to_bytes(),
            enc_isk,
            created_at: now_ms(),
            name,
            avatar: None,
            avatar_revision: 0, bio: String::new(), profile_revision: 0,
        })?;
        Ok(())
    }

    /// A bearer invite: whoever holds it may add us until it expires.
    pub fn mint_invite() -> Result<Invite> {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;

        let mut id = [0u8; 16];
        OsRng.fill_bytes(&mut id);
        let expiry_ms = now_ms() + INVITE_TTL_MS;

        let msg = invite_signing_input(MLS_WIRE_VERSION, &id, expiry_ms);
        let sig = IdentitySigner::sign(&msg)?;

        Ok(Invite { id: id.into(), expiry_ms, sig: sig.to_bytes().into() })
    }

    /// The whole anti-spam gate for pairing; no server is trusted. Callers spend the invite only on
    /// success, so a failed accept does not burn it.
    pub fn verify_invite(invite: &Invite) -> bool {
        let Some(our_ipk) = Identity::local_ipk() else {
            return false;
        };
        // `expiry_ms` is scan-time freshness. Accepting also allows for a pairing Welcome that sat
        // in the home stash for up to WELCOME_LIFETIME_MS.
        if now_ms() >= Self::invite_unusable_at(invite) {
            return false;
        }
        if Self::invite_is_spent(&invite.id.0) {
            return false;
        }
        let msg = invite_signing_input(MLS_WIRE_VERSION, &invite.id.0, invite.expiry_ms);
        verify_ed25519(&our_ipk, &msg, &invite.sig.0).is_ok()
    }

    fn invite_unusable_at(invite: &Invite) -> u64 {
        invite.expiry_ms.saturating_add(WELCOME_LIFETIME_MS)
    }

    fn invite_is_spent(id: &[u8; 16]) -> bool {
        let conn = core().db.identity().lock();
        conn.query_row("SELECT 1 FROM spent_invite WHERE id = ?1", [&id[..]], |_| Ok(()))
            .is_ok()
    }

    pub fn spend_invite(invite: &Invite) {
        let conn = core().db.identity().lock();
        let now = now_ms();
        let _ = conn.execute("DELETE FROM spent_invite WHERE unusable_at_ms <= ?1", [now]);
        let _ = conn.execute(
            "INSERT OR IGNORE INTO spent_invite(id, unusable_at_ms) VALUES (?1, ?2)",
            rusqlite::params![&invite.id.0[..], Self::invite_unusable_at(invite)],
        );
    }

    pub fn public_key() -> rusqlite::Result<PublicKey> {
        let conn = core().db.identity().lock();
        conn.query_one("SELECT ipk FROM identity WHERE id = 0", [], |row| {
            row.get("ipk")
                .map(|k: [u8; 32]| PublicKey::from_bytes(&k).expect("not a ed25519 public key"))
        })
    }

    /// `pub(super)`, so the raw secret bytes never leave `data`.
    pub(super) fn secret_key_with_manager() -> Result<Zeroizing<SecretKey>> {
        let current_ipk = Identity::public_key()?.to_bytes();
        cached_or_open(&ISK_CACHE, current_ipk, || {
            let store = core().secure_store.get().ok_or(anyhow!("API is not initialized"))?;
            let conn = core().db.identity().lock();
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
    pub fn sign(message: &[u8]) -> Result<Signature> {
        let secret = Identity::secret_key_with_manager()?;
        let key = SigningKey::from_bytes(&secret);
        Ok(key.sign(message))
    }

    /// Deterministic sub-key, so TLS transcripts and application messages never share a signer.
    pub fn tls_subkey() -> Result<SigningKey> {
        let secret = Identity::secret_key_with_manager()?;
        let public = SigningKey::from_bytes(&secret).verifying_key();
        Ok(derive_p2p_tls_key(&secret, public.as_bytes()))
    }

    pub fn sign_with_ipk(message: &[u8]) -> Result<(Signature, [u8; 32])> {
        let secret = Identity::secret_key_with_manager()?;
        let key = SigningKey::from_bytes(&secret);
        let pub_bytes = key.verifying_key().to_bytes();
        Ok((key.sign(message), pub_bytes))
    }
}

/// The only bare `SigningKey` outside `data`, for sends that sign many times; drop it promptly.
/// `expected_ipk` stops a caller with stale identity state from signing with another secret.
pub(crate) fn secret_key_signing(expected_ipk: &[u8; 32]) -> Result<SigningKey> {
    let secret = Identity::secret_key_with_manager()?;
    let key = SigningKey::from_bytes(&secret);
    if &key.verifying_key().to_bytes() != expected_ipk {
        return Err(anyhow!("identity secret does not match expected IPK"));
    }
    Ok(key)
}

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

pub(super) fn set_details_tx(
    conn: &rusqlite::Connection, name: &str, bio: &str, now_ms: u64,
) -> Result<()> {
    let name = validate_nickname(name).map_err(|e| anyhow!(e))?;
    anyhow::ensure!(bio.chars().count() <= 160, "Bio is limited to 160 characters");
    let old: u64 =
        conn.query_row("SELECT profile_revision FROM identity WHERE id = 0", [], |r| r.get(0))?;
    let revision = old.max(now_ms).checked_add(1).ok_or_else(|| anyhow!("revision overflow"))?;
    conn.execute(
        "UPDATE identity SET name = ?1, bio = ?2, profile_revision = ?3 WHERE id = 0",
        (name, bio.trim(), revision),
    )?;
    Ok(())
}

/// Wall time lets a fresh restore supersede its old profile; the persisted
/// counter also advances when changes share a millisecond or the clock recedes.
pub(super) fn set_avatar_tx(
    conn: &rusqlite::Connection, avif: Option<&[u8]>, now_ms: u64,
) -> Result<u64> {
    if let Some(bytes) = avif {
        crate::data::peer_avatar::check_avif(bytes)?;
    }
    let previous: u64 = conn.query_row(
        "SELECT avatar_revision FROM identity WHERE id = 0", [], |r| r.get(0),
    )?;
    let next = previous.checked_add(1).ok_or_else(|| anyhow!("profile revision exhausted"))?;
    let revision = now_ms.max(next);
    let stored_revision = i64::try_from(revision)?;
    conn.execute(
        "UPDATE identity SET avatar = ?1, avatar_revision = ?2 WHERE id = 0",
        (avif, stored_revision),
    )?;
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::data::identity;
    use crate::test_support::data::open;

    #[test]
    fn revisions_advance_with_equal_or_backwards_clocks() {
        let conn = open(crate::db::identity::migrate);
        identity(&conn, 1);
        let image = b"\0\0\0\x0cftypavif";
        assert_eq!(set_avatar_tx(&conn, Some(image), 100).unwrap(), 100);
        assert_eq!(set_avatar_tx(&conn, None, 100).unwrap(), 101);
        assert_eq!(set_avatar_tx(&conn, Some(image), 90).unwrap(), 102);
        let row = conn.query_row("SELECT * FROM identity", [], IdentityRow::from_row).unwrap();
        assert_eq!(row.avatar_revision, 102);
        assert_eq!(row.avatar.as_deref(), Some(image.as_slice()));
    }
}
