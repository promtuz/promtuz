//! Encrypted data backup (IDENTITY_RECOVERY.md §4): history + contacts +
//! name, sealed under a key derived from the isk — restoring identity
//! through either channel auto-unlocks it, no separate password.
//!
//! Blob: `b"PZBK" ‖ version:u8 ‖ nonce:24 ‖ XChaCha20-Poly1305(lz4(postcard))`.
//! Decrypt authenticates BEFORE decompress, so the lz4 size prefix is
//! trusted input. lz4 over zstd on purpose — pure Rust, no NDK C dep.

use anyhow::Result;
use anyhow::anyhow;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::KeyInit;
use hkdf::Hkdf;
use serde::Deserialize;
use serde::Serialize;
use sha2::Sha256;

use crate::data::contact::Contact;
use crate::data::identity::Identity;
use crate::data::message::Message;
use crate::data::reaction::Reaction;
use crate::db::messages::MessageRow;
use crate::db::messages::ReactionRow;
use crate::db::peers::ContactRow;

const MAGIC: &[u8; 4] = b"PZBK";
const VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
struct BackupPayload {
    name:      String,
    contacts:  Vec<ContactRow>,
    messages:  Vec<MessageRow>,
    reactions: Vec<ReactionRow>,
}

/// `HKDF-SHA256(isk, "promtuz-backup-v1")` — the spec §4 label, verbatim.
fn backup_key(isk: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, isk);
    let mut okm = [0u8; 32];
    hk.expand(b"promtuz-backup-v1", &mut okm).expect("32 bytes is a valid HKDF length");
    okm
}

fn encode(key: &[u8; 32], payload: &BackupPayload) -> Result<Vec<u8>> {
    let plain = postcard::to_allocvec(payload).map_err(|e| anyhow!("encode payload: {e}"))?;
    let compressed = lz4_flex::compress_prepend_size(&plain);

    let mut nonce = [0u8; 24];
    {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;
        OsRng.fill_bytes(&mut nonce);
    }
    let ct = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), compressed.as_slice())
        .map_err(|_| anyhow!("encrypt failed"))?;

    let mut out = Vec::with_capacity(4 + 1 + 24 + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decode(key: &[u8; 32], blob: &[u8]) -> Result<BackupPayload> {
    let rest = blob.strip_prefix(MAGIC.as_slice()).ok_or_else(|| anyhow!("not a backup blob"))?;
    let (&version, rest) = rest.split_first().ok_or_else(|| anyhow!("truncated blob"))?;
    if version != VERSION {
        return Err(anyhow!("unsupported backup version {version}"));
    }
    if rest.len() < 24 {
        return Err(anyhow!("truncated blob"));
    }
    let (nonce, ct) = rest.split_at(24);

    let compressed = XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| anyhow!("decrypt failed — wrong identity or corrupted blob"))?;
    let plain = lz4_flex::decompress_size_prepended(&compressed)
        .map_err(|e| anyhow!("decompress: {e}"))?;
    postcard::from_bytes(&plain).map_err(|e| anyhow!("decode payload: {e}"))
}

/// Snapshot everything restorable into one encrypted blob. The platform
/// owns cadence (daily / dirty-flag via `on_db_changed`) and placement
/// (Drive app-folder / iCloud).
pub fn export() -> Result<Vec<u8>> {
    let identity = Identity::get().ok_or_else(|| anyhow!("no identity"))?;
    let payload = BackupPayload {
        name:      identity.name(),
        contacts:  Contact::list(),
        messages:  Message::dump_all(),
        reactions: Reaction::dump_all(),
    };
    let secret = Identity::secret_key_with_manager()?;
    encode(&backup_key(&secret), &payload)
}

/// Restore a blob into the local DBs. Requires the identity to already be
/// restored (the key derives from the isk). Idempotent — upserts throughout.
pub fn import(blob: &[u8]) -> Result<()> {
    let secret = Identity::secret_key_with_manager()?;
    let payload = decode(&backup_key(&secret), blob)?;

    let contacts = Contact::import_rows(&payload.contacts)?;
    let messages = Message::import_rows(&payload.messages)?;
    let reactions = Reaction::import_rows(&payload.reactions)?;
    Identity::set_name(&payload.name)?;

    log::info!("BACKUP: imported {contacts} contacts, {messages} messages, {reactions} reactions");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> BackupPayload {
        BackupPayload {
            name:      "bhuv".into(),
            contacts:  vec![ContactRow {
                ipk:          [3u8; 32],
                name:         "alice".into(),
                added_at:     42,
                mls_group_id: Some([9u8; 32]),
            }],
            messages:  Vec::new(),
            reactions: Vec::new(),
        }
    }

    #[test]
    fn blob_roundtrips() {
        let key = backup_key(&[7u8; 32]);
        let blob = encode(&key, &payload()).unwrap();
        let back = decode(&key, &blob).unwrap();
        assert_eq!(back.name, "bhuv");
        assert_eq!(back.contacts.len(), 1);
        assert_eq!(back.contacts[0].mls_group_id, Some([9u8; 32]));
    }

    #[test]
    fn tampered_blob_fails_auth() {
        let key = backup_key(&[7u8; 32]);
        let mut blob = encode(&key, &payload()).unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 1;
        assert!(decode(&key, &blob).is_err());
    }

    #[test]
    fn wrong_isk_cannot_open() {
        let blob = encode(&backup_key(&[7u8; 32]), &payload()).unwrap();
        assert!(decode(&backup_key(&[8u8; 32]), &blob).is_err());
    }

    #[test]
    fn different_isks_derive_different_keys() {
        assert_ne!(backup_key(&[1u8; 32]), backup_key(&[2u8; 32]));
    }
}
