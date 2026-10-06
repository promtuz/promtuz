//! Blob: `b"PZBK" ‖ version:u8 ‖ nonce:24 ‖ XChaCha20-Poly1305(lz4(postcard))`. Decryption
//! authenticates before decompressing, so the lz4 size prefix is trusted input.

use anyhow::Result;
use anyhow::anyhow;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::XNonce;
use chacha20poly1305::aead::Aead;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::aead::KeyInit;
use hkdf::Hkdf;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::Sha256;


use crate::data::contact::Contact;
use crate::data::conversation::Conversation;
use crate::data::identity::Identity;
use crate::data::media::MediaBackupRow;
use crate::data::media::StickerRefBackup;
use crate::data::message::Message;
use crate::data::reaction::Reaction;
use crate::data::stickers::PackBackup;
use crate::db::Stores;
use crate::db::from_row;
use crate::db::identity::IdentityRow;
use crate::db::messages::ConversationRow;
use crate::db::messages::MemberRow;
use crate::db::messages::MessageRow;
use crate::db::messages::ReactionRow;
use crate::db::one;
use crate::db::peers::ContactRow;
use crate::state::core;

const MAGIC: &[u8; 4] = b"PZBK";
/// A reshaped row needs a new version: postcard would decode an old blob into garbage.
const VERSION: u8 = 3;

#[derive(Serialize, Deserialize)]
struct BackupPayload {
    name:          String,
    contacts:      Vec<ContactRow>,
    conversations: Vec<ConversationRow>,
    members:       Vec<MemberRow>,
    messages:      Vec<MessageRow>,
    reactions:     Vec<ReactionRow>,
    media:         Vec<MediaBackupRow>,
    read_state:    Vec<ReadRow>,
    member_read:   Vec<MemberReadRow>,
    prefs:         Vec<(String, String)>,
}

/// Optional suffixes after the payload, in format order. Postcard is not self-describing, so new
/// data gets a new trailing suffix, and an older blob decodes the ones it lacks at default.
#[derive(Default)]
struct Suffixes {
    stickers: (Vec<StickerRefBackup>, Vec<PackBackup>),
    avatar:   Option<Vec<u8>>,
    groups:   Vec<crate::data::group_picture::Backup>,
    receipts: crate::data::receipts::Backup,
}

#[derive(Serialize, Deserialize)]
pub struct ReadRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id:  [u8; 16],
    pub upto_dispatch_id: Vec<u8>,
}

from_row!(ReadRow { conversation_id, upto_dispatch_id });

#[derive(Serialize, Deserialize)]
pub struct MemberReadRow {
    #[serde(with = "serde_bytes")]
    pub conversation_id:  [u8; 16],
    #[serde(with = "serde_bytes")]
    pub member_ipk:       [u8; 32],
    pub upto_dispatch_id: Vec<u8>,
}

from_row!(MemberReadRow { conversation_id, member_ipk, upto_dispatch_id });

/// The label stays `v1` across blob versions; changing it orphans every existing backup.
fn backup_key(isk: &[u8; 32]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(None, isk);
    let mut okm = [0u8; 32];
    hk.expand(b"promtuz-backup-v1", &mut okm).expect("32 bytes is a valid HKDF length");
    okm
}

fn encode(key: &[u8; 32], payload: &BackupPayload, suffixes: &Suffixes) -> Result<Vec<u8>> {
    let Suffixes { stickers, avatar, groups, receipts } = suffixes;
    let plain = postcard::to_allocvec(&(payload, stickers, avatar, groups, receipts))
        .map_err(|e| anyhow!("encode: {e}"))?;
    let compressed = lz4_flex::compress_prepend_size(&plain);
    drop(plain);

    let mut nonce = [0u8; 24];
    {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;
        OsRng.fill_bytes(&mut nonce);
    }
    let header = MAGIC.len() + 1 + nonce.len();
    let mut out = Vec::with_capacity(header + compressed.len() + 16);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&compressed);
    drop(compressed);
    let tag = XChaCha20Poly1305::new(key.into())
        .encrypt_in_place_detached(XNonce::from_slice(&nonce), b"", &mut out[header..])
        .map_err(|_| anyhow!("encrypt failed"))?;
    out.extend_from_slice(&tag);
    Ok(out)
}

fn decode(key: &[u8; 32], blob: &[u8]) -> Result<(BackupPayload, Suffixes)> {
    split(&unseal(key, blob)?)
}

fn unseal(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>> {
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
    lz4_flex::decompress_size_prepended(&compressed).map_err(|e| anyhow!("decompress: {e}"))
}

fn split(plain: &[u8]) -> Result<(BackupPayload, Suffixes)> {
    let (payload, mut rest) =
        postcard::take_from_bytes(plain).map_err(|e| anyhow!("decode payload: {e}"))?;
    let suffixes = Suffixes {
        stickers: next(&mut rest, "stickers")?,
        avatar:   next(&mut rest, "avatar")?,
        groups:   next(&mut rest, "groups")?,
        receipts: next(&mut rest, "receipts")?,
    };
    Ok((payload, suffixes))
}

fn next<T: DeserializeOwned + Default>(rest: &mut &[u8], what: &str) -> Result<T> {
    if rest.is_empty() {
        return Ok(T::default());
    }
    let (value, tail) =
        postcard::take_from_bytes(rest).map_err(|e| anyhow!("decode {what}: {e}"))?;
    *rest = tail;
    Ok(value)
}

pub fn export() -> Result<Vec<u8>> {
    let secret = Identity::secret_key_with_manager()?;
    export_from(&core().db, &backup_key(&secret))
}

/// Takes one database lock at a time; messages.db is read under one lock, as one snapshot.
fn export_from(db: &Stores, key: &[u8; 32]) -> Result<Vec<u8>> {
    let sql = "SELECT * FROM identity WHERE id = 0";
    let identity = one(&db.identity().lock(), sql, [], IdentityRow::from_row)?
        .ok_or_else(|| anyhow!("no identity"))?;
    let contacts = Contact::dump_all_tx(&db.contacts().lock())?;
    let conn = db.messages().lock();
    let (conversations, members) = Conversation::dump_all_tx(&conn)?;
    let (read_state, member_read) = crate::data::message::dump_read_state_tx(&conn)?;
    let mut prefs = crate::data::app_prefs::dump_all_tx(&conn)?;
    // A restored device has no KeyPackage private keys, and a pre-backup refresh nonce
    // could ask peers for an already-consumed Welcome, so refresh state stays behind.
    prefs.retain(|(k, _)| k != "profile_bio" && !k.starts_with("group_refresh:"));
    prefs.push(("profile_bio".into(), identity.bio));
    let payload = BackupPayload {
        name: identity.name,
        contacts,
        conversations,
        members,
        messages: Message::dump_all_tx(&conn)?,
        reactions: Reaction::dump_all_tx(&conn)?,
        media: crate::data::media::dump_all_tx(&conn)?,
        read_state,
        member_read,
        prefs,
    };
    let suffixes = Suffixes {
        stickers: (
            crate::data::media::dump_sticker_refs_tx(&conn)?,
            crate::data::stickers::dump_all_tx(&conn)?,
        ),
        avatar:   identity.avatar,
        groups:   crate::data::group_picture::dump_tx(&conn)?,
        receipts: crate::data::receipts::dump_tx(&conn)?,
    };
    drop(conn);
    encode(key, &payload, &suffixes)
}

#[derive(uniffi::Record)]
pub struct BackupMergeReport {
    pub version:           u8,
    pub blob_bytes:        u64,
    /// Reported, never applied.
    pub backup_name:       String,
    pub current_name:      String,
    pub contacts_in_blob:  u32,
    pub contacts_added:    u32,
    pub messages_in_blob:  u32,
    pub messages_added:    u32,
    pub reactions_in_blob: u32,
    pub reactions_added:   u32,
    /// Zero on a blob written before v3, and the tell that its messages have
    /// no chats to restore into.
    pub conversations_in_blob: u32,
    pub conversations_added:   u32,
    pub media_in_blob:         u32,
    pub media_added:           u32,
}

/// `replace` lets the blob's profile, contacts and reactions replace live ones. Without it no live
/// row is replaced and our profile is untouched, as whoever holds the isk can edit the blob.
pub fn import(blob: &[u8], replace: bool) -> Result<BackupMergeReport> {
    let secret = Identity::secret_key_with_manager()?;
    let report = import_into(&core().db, &backup_key(&secret), blob, replace);
    crate::data::peer_avatar::notify_changed();
    crate::profile_sync::store::wake();
    report
}

/// messages.db takes the blob in one transaction. contacts.db and the identity are separate
/// databases and commit on their own.
fn import_into(
    db: &Stores, key: &[u8; 32], blob: &[u8], replace: bool,
) -> Result<BackupMergeReport> {
    let (payload, suffixes) = decode(key, blob)?;

    let contacts = {
        let mut conn = db.contacts().lock();
        let tx = conn.transaction()?;
        let n = Contact::import_rows_tx(&tx, &payload.contacts, replace)?;
        tx.commit()?;
        n
    };
    let mut conn = db.messages().lock();
    let tx = conn.transaction()?;
    // Conversations first: everything below hangs off them.
    let conversations =
        Conversation::import_rows_tx(&tx, &payload.conversations, &payload.members)?;
    let messages = Message::import_rows_tx(&tx, &payload.messages)?;
    let reactions = Reaction::import_rows_tx(&tx, &payload.reactions, replace)?;
    let media = crate::data::media::import_rows_tx(&tx, &payload.media)?;
    crate::data::media::import_sticker_refs_tx(&tx, &suffixes.stickers.0)?;
    crate::data::stickers::import_rows_tx(&tx, &suffixes.stickers.1)?;
    crate::data::message::import_read_state_tx(&tx, &payload.read_state, &payload.member_read)?;
    crate::data::receipts::backfill_tx(&tx)?;
    crate::data::receipts::restore_tx(&tx, &suffixes.receipts)?;
    crate::data::app_prefs::import_rows_tx(&tx, &payload.prefs)?;
    crate::data::group_picture::restore_tx(&tx, &suffixes.groups)?;
    tx.commit()?;
    drop(conn);

    let identity = db.identity().lock();
    if replace {
        let bio = payload.prefs.iter().find(|(k, _)| k == "profile_bio");
        crate::data::identity::set_details_tx(
            &identity,
            &payload.name,
            bio.map_or("", |(_, v)| v.as_str()),
        )?;
        // The picture can fail its own gate, and the restore must not fail over it.
        if let Err(e) =
            crate::data::identity::set_avatar_tx(&identity, suffixes.avatar.as_deref())
        {
            log::warn!("BACKUP: could not restore the profile picture: {e}");
        }
    }
    let current_name: Option<String> =
        one(&identity, "SELECT name FROM identity WHERE id = 0", [], |r| r.get(0))?;
    drop(identity);

    log::info!(
        "BACKUP: {} {contacts} contacts, {conversations} conversations, \
         {messages} messages, {reactions} reactions, {media} media",
        if replace { "imported" } else { "merged" },
    );
    Ok(BackupMergeReport {
        version: VERSION,
        blob_bytes: blob.len() as u64,
        backup_name: payload.name,
        current_name: current_name.unwrap_or_default(),
        contacts_in_blob: payload.contacts.len() as u32,
        contacts_added: contacts as u32,
        messages_in_blob: payload.messages.len() as u32,
        messages_added: messages as u32,
        reactions_in_blob: payload.reactions.len() as u32,
        reactions_added: reactions as u32,
        conversations_in_blob: payload.conversations.len() as u32,
        conversations_added: conversations as u32,
        media_in_blob: payload.media.len() as u32,
        media_added: media as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::data::identity;
    use crate::test_support::data::stores;

    /// The plaintext a released build writes, every row type filled once. It must keep decoding;
    /// a reshaped row needs a new `VERSION` instead.
    const GOLDEN: &str = concat!(
        "04426875760102020202020202020202020202020202020202020202020202020202020202020341",
        "646180e2cfaa06010303030303030303030303030303030303030303030303030303030303030303",
        "01000110040404040404040404040404040404040100e4e2cfaa060109426f6f6b20636c75620120",
        "050505050505050505050505050505050505050505050505050505050505050580e2cfaa06012002",
        "02020202020202020202020202020202020202020202020202020202020202011004040404040404",
        "04040404040404040420020202020202020202020202020202020202020202020202020202020202",
        "02020280e2cfaa0601021a3030303030303030303130303030303030303030303030303030100404",
        "04040404040404040404040404040120020202020202020202020202020202020202020202020202",
        "02020202020202020568656c6c6f0081e2cfaa060101100606060606060606060606060606060600",
        "0000001a303030303030303030323030303030303030303030303030303010040404040404040404",
        "0404040404040400066869204164610182e2cfaa0604011007070707070707070707070707070707",
        "01000110060606060606060606060606060606060001100404040404040404040404040404040410",
        "06060606060606060606060606060606200808080808080808080808080808080808080808080808",
        "08080808080808080804f09f918d83e2cfaa06011004040404040404040404040404040404100707",
        "070707070707070707070707070701000a696d6167652f6176696600030202010301020301020405",
        "01200909090909090909090909090909090909090909090909090909090909090909dc0b01100404",
        "04040404040404040404040404041006060606060606060606060606060606011004040404040404",
        "04040404040404040420020202020202020202020202020202020202020202020202020202020202",
        "0202100707070707070707070707070707070702057468656d65046461726b0b70726f66696c655f",
        "62696f0752656164696e670104040404040404040404040404040404070707070707070707070707",
        "0707070704abababab010a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a010b0b0b0b0b0b0b0b0b0b0b0b0b",
        "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c",
        "0c0c0c0c0c0c0c0c0c0c0c02044361747384e2cfaa06010a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0d",
        "0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0080048004010c0000",
        "000c6674797061766966010404040404040404040404040404040403010c0000000c667479706176",
        "6966011a303030303030303030323030303030303030303030303030303001011a30303030303030",
        "30303230303030303030303030303030303030020202020202020202020202020202020202020202",
        "020202020202020202020201010a010b010c00011a30303030303030303031303030303030303030",
        "30303030303030010d010e0100010404040404040404040404040404040402020202020202020202",
        "02020202020202020202020202020202020202020202",
    );
    /// Where blobs of each vintage end: the payload alone, then with stickers, the avatar, group
    /// pictures and receipts.
    const VINTAGES: [usize; 5] = [771, 956, 970, 1002, 1182];

    fn golden() -> Vec<u8> {
        hex::decode(GOLDEN).unwrap()
    }

    fn parts(s: &Suffixes) -> [Vec<u8>; 4] {
        [
            postcard::to_allocvec(&s.stickers).unwrap(),
            postcard::to_allocvec(&s.avatar).unwrap(),
            postcard::to_allocvec(&s.groups).unwrap(),
            postcard::to_allocvec(&s.receipts).unwrap(),
        ]
    }

    /// A fresh device that already holds the restored identity, and the key of its blobs.
    fn device() -> (Stores, tempfile::TempDir, [u8; 32]) {
        let (db, dir) = stores();
        let key = backup_key(&identity(&db.identity().lock(), 1).to_bytes());
        (db, dir, key)
    }

    #[test]
    fn blobs_of_every_vintage_still_decode() {
        let plain = golden();
        let defaults = parts(&Suffixes::default());
        for (present, &end) in VINTAGES.iter().enumerate() {
            let (payload, suffixes) = split(&plain[..end]).unwrap();
            assert_eq!(postcard::to_allocvec(&payload).unwrap(), &plain[..VINTAGES[0]]);
            for (n, part) in parts(&suffixes).iter().enumerate() {
                let expected = if n < present {
                    &plain[VINTAGES[n]..VINTAGES[n + 1]]
                } else {
                    &defaults[n][..]
                };
                assert_eq!(part, expected, "suffix {n} of a blob with {present}");
            }
        }
        let (payload, suffixes) = split(&plain).unwrap();
        assert_eq!((payload.name.as_str(), payload.contacts[0].name.as_str()), ("Bhuv", "Ada"));
        let texts: Vec<_> = payload.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(texts, ["hello", "hi Ada"]);
        assert_eq!(payload.media[0].duration_ms, 1500);
        assert_eq!(suffixes.stickers.1[0].pack.name, "Cats");
    }

    /// v2 restored messages without their chats, and every chat read empty. A restore brings
    /// back every table, so the restored device exports the same plaintext.
    #[test]
    fn a_blob_carrying_messages_carries_their_conversations() {
        let (db, _dir, key) = device();
        let (payload, suffixes) = split(&golden()).unwrap();
        let blob = encode(&key, &payload, &suffixes).unwrap();
        let report = import_into(&db, &key, &blob, true).unwrap();
        assert_eq!((report.conversations_added, report.messages_added), (1, 2));
        let chat = payload.conversations[0].id;
        let shown = Message::get_messages_tx(&db.messages().lock(), &chat, 10, "").unwrap();
        assert_eq!(shown.len(), 2, "the restored chat shows its messages");
        assert_eq!(unseal(&key, &export_from(&db, &key).unwrap()).unwrap(), golden());
    }

    #[test]
    fn a_group_picture_without_its_chat_does_not_abort_the_import() {
        let (db, _dir, key) = device();
        let (payload, mut suffixes) = split(&golden()).unwrap();
        let orphan = crate::data::group_picture::Backup {
            conversation: [0xEE; 16],
            revision:     1,
            avif:         None,
        };
        suffixes.groups.insert(0, orphan);
        import_into(&db, &key, &encode(&key, &payload, &suffixes).unwrap(), true).unwrap();
        let restored = unseal(&key, &export_from(&db, &key).unwrap()).unwrap();
        assert_eq!(restored, golden(), "everything else is restored and the orphan is dropped");
    }

    #[test]
    fn a_tampered_blob_an_old_version_or_another_key_is_refused_before_decompression() {
        let key = backup_key(&[7; 32]);
        let (payload, suffixes) = split(&golden()).unwrap();
        let blob = encode(&key, &payload, &suffixes).unwrap();
        let with = |at: usize, byte: u8| {
            let mut b = blob.clone();
            b[at] = byte;
            b
        };
        for (blob, key, refusal) in [
            (with(40, blob[40] ^ 1), key, "decrypt failed"),
            (with(blob.len() - 1, blob[blob.len() - 1] ^ 1), key, "decrypt failed"),
            (blob.clone(), backup_key(&[8; 32]), "decrypt failed"),
            (with(4, 2), key, "unsupported backup version 2"),
        ] {
            let e = unseal(&key, &blob).unwrap_err().to_string();
            assert!(e.starts_with(refusal), "{e}");
        }
    }
}
