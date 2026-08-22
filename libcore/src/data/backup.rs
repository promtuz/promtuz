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
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::media::MediaBackupRow;
use crate::data::message::Message;
use crate::data::reaction::Reaction;
use crate::db::messages::ConversationRow;
use crate::db::messages::MemberRow;
use crate::db::messages::MessageRow;
use crate::db::messages::ReactionRow;
use crate::db::peers::ContactRow;

const MAGIC: &[u8; 4] = b"PZBK";
/// 2: the conversation re-key. `MessageRow` and `ReactionRow` are serialized
/// structurally, so moving them off `peer_ipk` onto `conversation_id` (plus the
/// new `sender_ipk`) changes the blob's shape. A v1 blob decodes into garbage
/// rather than failing loudly, so the version gate is what keeps a stale backup
/// from silently importing conversation ids that were somebody's public key.
///
/// 3: carries the conversations themselves. v2 saved messages keyed on
/// conversations it did not save, so a restore produced a full messages table
/// that nothing could reach — every chat read as empty and the home list was
/// blank. Media rows and read state travel with them.
const VERSION: u8 = 3;

#[derive(Serialize, Deserialize)]
struct BackupPayload {
    name:          String,
    contacts:      Vec<ContactRow>,
    /// The chats themselves. Without these the messages below are orphans: they
    /// name a conversation id, and every read starts from the conversation.
    conversations: Vec<ConversationRow>,
    /// Rosters. A group with no members can't be shown, and can't be sent to —
    /// the fan-out list comes from here.
    members:       Vec<MemberRow>,
    messages:      Vec<MessageRow>,
    reactions:     Vec<ReactionRow>,
    /// Pictures and attachment stubs. The inline image bytes live here, so this
    /// is what makes a restored photo a photo rather than an empty caption.
    media:         Vec<MediaBackupRow>,
    /// How far we had read, and how far each member had. Purely cosmetic, but
    /// losing it makes every chat in a restored app scream unread.
    read_state:    Vec<ReadRow>,
    member_read:   Vec<MemberReadRow>,
    /// App-wide settings. They used to sit in platform preferences, which the
    /// backup rules do not ship, so a reinstall reset them every time.
    prefs:         Vec<(String, String)>,
}

/// Our own read watermark for a conversation.
#[derive(Serialize, Deserialize)]
pub struct ReadRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id:  [u8; 16],
    pub upto_dispatch_id: Vec<u8>,
}

/// A member's read watermark — the group tick advances at the slowest of them.
#[derive(Serialize, Deserialize)]
pub struct MemberReadRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id:  [u8; 16],
    #[serde(with = "serde_bytes")]
    pub member_ipk:       [u8; 32],
    pub upto_dispatch_id: Vec<u8>,
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
    let (conversations, members) = Conversation::dump_all();
    let (read_state, member_read) = crate::data::message::dump_read_state();
    let payload = BackupPayload {
        name: identity.name(),
        contacts: Contact::list(),
        conversations,
        members,
        messages: Message::dump_all(),
        reactions: Reaction::dump_all(),
        media: crate::data::media::dump_all(),
        read_state,
        member_read,
        prefs: crate::data::app_prefs::dump_all(),
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
    // Conversations first: everything below hangs off them.
    let conversations = Conversation::import_rows(&payload.conversations, &payload.members)?;
    let messages = Message::import_rows(&payload.messages)?;
    let reactions = Reaction::import_rows(&payload.reactions)?;
    let media = crate::data::media::import_rows(&payload.media)?;
    crate::data::message::import_read_state(&payload.read_state, &payload.member_read)?;
    crate::data::app_prefs::import_rows(&payload.prefs)?;
    Identity::set_name(&payload.name)?;

    log::info!(
        "BACKUP: imported {contacts} contacts, {conversations} conversations, \
         {messages} messages, {reactions} reactions, {media} media"
    );
    Ok(())
}

/// What [`import_merge`] found and what it did with it — one number per
/// table so the caller can print a full account rather than "ok".
#[derive(Debug, Default, Clone)]
pub struct MergeReport {
    pub version:           u8,
    pub blob_bytes:        u64,
    /// Display name sealed in the blob, and the one currently live. Reported
    /// only — a merge never renames the identity.
    pub backup_name:       String,
    pub current_name:      String,
    pub contacts_in_blob:  u32,
    pub contacts_added:    u32,
    pub messages_in_blob:  u32,
    pub messages_added:    u32,
    pub reactions_in_blob: u32,
    pub reactions_added:   u32,
    /// The chats themselves. Zero here on a blob older than v3 is the tell
    /// that its messages will restore into nothing.
    pub conversations_in_blob: u32,
    pub conversations_added:   u32,
    pub media_in_blob:         u32,
    pub media_added:           u32,
}

/// Additive restore for the Backup & Restore dev screen: every row the blob
/// carries that we do NOT already have is inserted; nothing existing is
/// touched, deleted or renamed.
///
/// Distinct from [`import`] on purpose. `import` is the reinstall path, where
/// the blob IS the source of truth and replacing is correct because there is
/// nothing live to clobber. Here there is: the blob decrypts under a key the
/// identity owner holds, so its plaintext is editable by that owner and must
/// never outrank a live row. Hence `INSERT OR IGNORE` on every table and no
/// [`Identity::set_name`] — existing state always wins a collision.
pub fn import_merge(blob: &[u8]) -> Result<MergeReport> {
    let secret = Identity::secret_key_with_manager()?;
    let payload = decode(&backup_key(&secret), blob)?;

    let contacts_added = Contact::merge_rows(&payload.contacts)?;
    let conversations_added =
        Conversation::import_rows(&payload.conversations, &payload.members)?;
    let messages_added = Message::merge_rows(&payload.messages)?;
    let reactions_added = Reaction::merge_rows(&payload.reactions)?;
    let media_added = crate::data::media::import_rows(&payload.media)?;
    crate::data::app_prefs::import_rows(&payload.prefs)?;

    let report = MergeReport {
        version: VERSION,
        blob_bytes: blob.len() as u64,
        backup_name: payload.name,
        current_name: Identity::get().map(|i| i.name()).unwrap_or_default(),
        contacts_in_blob: payload.contacts.len() as u32,
        contacts_added: contacts_added as u32,
        messages_in_blob: payload.messages.len() as u32,
        messages_added: messages_added as u32,
        reactions_in_blob: payload.reactions.len() as u32,
        reactions_added: reactions_added as u32,
        conversations_in_blob: payload.conversations.len() as u32,
        conversations_added: conversations_added as u32,
        media_in_blob: payload.media.len() as u32,
        media_added: media_added as u32,
    };
    log::info!("BACKUP: merge {report:?}");
    Ok(report)
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
                mls_group_id:  Some([9u8; 32]),
                status:        1,
                reject_reason: None,
            }],
            conversations: Vec::new(),
            members:       Vec::new(),
            messages:      Vec::new(),
            reactions:     Vec::new(),
            media:         Vec::new(),
            read_state:    Vec::new(),
            member_read:   Vec::new(),
            prefs:         Vec::new(),
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

    /// The failure v2 shipped: messages name a conversation, so a blob that
    /// carries the messages without the conversations restores a full table
    /// nothing can reach — every chat reads empty and the home list is blank.
    /// Whatever else changes, these two travel together.
    #[test]
    fn a_blob_carrying_messages_carries_their_conversations() {
        let conv = [0xC1u8; 16];
        let mut p = payload();
        p.conversations = vec![ConversationRow {
            id:           conv,
            kind:         1,
            title:        "book club".into(),
            mls_group_id: Some(vec![0xAA; 32]),
            created_at:   100,
            created_by:   Some(vec![3u8; 32]),
            pinned:       true,
            muted:        false,
            alerted_at:   0,
        }];
        p.members = vec![MemberRow {
            conversation_id: conv,
            member_ipk:      [3u8; 32],
            role:            1,
            joined_at:       100,
            active:          true,
        }];

        let key = backup_key(&[7u8; 32]);
        let back = decode(&key, &encode(&key, &p).unwrap()).unwrap();

        assert_eq!(back.conversations.len(), 1, "the chat itself must survive the round trip");
        assert_eq!(back.conversations[0].id, conv);
        assert_eq!(back.conversations[0].title, "book club");
        assert_eq!(back.members.len(), 1, "and its roster, or it cannot be shown or sent to");
        assert_eq!(back.members[0].conversation_id, conv);
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
