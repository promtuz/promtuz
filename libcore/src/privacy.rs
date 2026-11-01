//! Local private history. Only the HPKE public key survives locking, allowing new arrivals to
//! be sealed without keeping a key capable of reading old history.
use crate::state::core;
use anyhow::{Result, anyhow, ensure};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hpke_rs::{HpkePrivateKey, HpkePublicKey};
use rusqlite::{Connection, OptionalExtension};
use zeroize::Zeroizing;
const PREFIX: &str = "pz-private-v1:";
const DOMAIN: &[u8] = b"promtuz-private-history-v1";

fn random<const N: usize>() -> [u8; N] {
    use ed25519_dalek::ed25519::signature::rand_core::{OsRng, RngCore};
    let mut v = [0; N];
    OsRng.fill_bytes(&mut v);
    v
}
fn derive(code: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    let mut key = Zeroizing::new([0; 32]);
    let params =
        argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|_| anyhow!("key parameters"))?;
    argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(code.as_bytes(), salt, key.as_mut())
        .map_err(|_| anyhow!("key derivation failed"))?;
    Ok(key)
}
pub(crate) fn is_private_tx(conn: &Connection, conv: &[u8; 16]) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM private_conversations WHERE conversation_id=?1)",
        [conv],
        |r| r.get(0),
    )?)
}
pub(crate) fn is_private(conv: &[u8; 16]) -> bool {
    is_private_tx(&core().db.messages().lock(), conv).unwrap_or(true)
}
pub(crate) fn require_access(conv: &[u8; 16]) -> Result<()> {
    ensure!(
        !is_private(conv) || core().private_sessions.read().contains_key(conv),
        "Chat is locked"
    );
    Ok(())
}
pub(crate) fn seal_bytes_tx(conn: &Connection, conv: &[u8; 16], bytes: &[u8]) -> Result<Vec<u8>> {
    let public: Option<Vec<u8>> = conn
        .query_row(
            "SELECT public_key FROM private_conversations WHERE conversation_id=?1",
            [conv],
            |r| r.get(0),
        )
        .optional()?;
    let Some(public) = public else { return Ok(bytes.to_vec()) };
    seal_with_public(conv, &public, bytes)
}
pub(crate) fn seal_with_public(conv: &[u8; 16], public: &[u8], bytes: &[u8]) -> Result<Vec<u8>> {
    let (enc, cipher) = crate::contact_card::hpke()
        .seal(&HpkePublicKey::new(public.to_vec()), DOMAIN, conv, bytes, None, None, None)
        .map_err(|_| anyhow!("private encryption failed"))?;
    Ok([PREFIX.as_bytes(), &postcard::to_allocvec(&(enc, cipher))?].concat())
}
pub(crate) fn open_bytes(conv: &[u8; 16], bytes: &[u8]) -> Result<Vec<u8>> {
    let cipher = bytes
        .strip_prefix(PREFIX.as_bytes())
        .ok_or_else(|| anyhow!("invalid private ciphertext"))?;
    let sessions = core().private_sessions.read();
    let key = sessions.get(conv).ok_or_else(|| anyhow!("Chat is locked"))?;
    let (enc, cipher): (Vec<u8>, Vec<u8>) = postcard::from_bytes(cipher)?;
    crate::contact_card::hpke()
        .open(&enc, &HpkePrivateKey::new(key.to_vec()), DOMAIN, conv, &cipher, None, None, None)
        .map_err(|_| anyhow!("private decryption failed"))
}
pub(crate) fn seal_tx(conn: &Connection, conv: &[u8; 16], content: &str) -> Result<String> {
    if !is_private_tx(conn, conv)? {
        return Ok(content.into());
    }
    Ok(format!("{PREFIX}{}", hex::encode(seal_bytes_tx(conn, conv, content.as_bytes())?)))
}
pub(crate) fn open_content(conv: &[u8; 16], content: &str) -> Result<String> {
    let cipher = content
        .strip_prefix(PREFIX)
        .ok_or_else(|| anyhow!("invalid private message ciphertext"))?;
    Ok(String::from_utf8(open_bytes(conv, &hex::decode(cipher)?)?)?)
}
pub(crate) fn ids_tx(conn: &Connection) -> Result<Vec<[u8; 16]>> {
    Ok(crate::db::all(conn, "SELECT conversation_id FROM private_conversations", [], |r| r.get(0))?)
}
pub(crate) fn hide(conv: &[u8; 16], code: &str) -> Result<()> {
    let _configuration = core().private_configuration.lock();
    let group = crate::data::conversation::Conversation::group_of(conv);
    let _operation = group.as_ref().map(|gid| crate::mls::recovery::operation_lock(gid).lock());
    ensure!(
        code.chars().count() >= 6 && code.len() <= 256,
        "Use a code with at least 6 characters"
    );
    // One exact trigger identifies one chat. Derive every candidate rather than storing a fast
    // password verifier, which would undermine Argon2's resistance to offline guessing.
    ensure!(
        find_code(
            code,
            false,
            core().private_generation.load(std::sync::atomic::Ordering::SeqCst)
        )?
        .is_none(),
        "That code already opens another chat"
    );
    let pair = crate::contact_card::hpke()
        .generate_key_pair()
        .map_err(|_| anyhow!("private key generation failed"))?;
    let salt = random::<16>();
    let key = derive(code, &salt)?;
    let nonce = random::<24>();
    let wrapped = XChaCha20Poly1305::new((&*key).into())
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload { msg: pair.private_key().as_slice(), aad: conv },
        )
        .map_err(|_| anyhow!("key encryption failed"))?;
    let mut conn = core().db.messages().lock();
    ensure!(!is_private_tx(&conn, conv)?, "Chat is already locked");
    ensure!(
        conn.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1)", [conv], |r| r
            .get::<_, bool>(
            0
        ))?,
        "Chat not found"
    );
    ensure!(!conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id=?1 AND outgoing=1 AND status=0)",[conv],|r|r.get::<_,bool>(0))?,"Wait for pending messages before locking this chat");
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO private_conversations VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            conv,
            salt.as_slice(),
            pair.public_key().as_slice(),
            [nonce.as_slice(), &wrapped].concat()
        ],
    )?;
    let messages: Vec<(String, String)> = crate::db::all(
        &tx,
        "SELECT id,content FROM messages WHERE conversation_id=?1",
        [conv],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    for (id, plain) in messages {
        tx.execute(
            "UPDATE messages SET content=?2 WHERE id=?1",
            (&id, seal_tx(&tx, conv, &plain)?),
        )?;
    }
    crate::data::media::privatize_tx(&tx, conv)?;
    crate::transfer::sharing::privatize_tx(&tx, conv)?;
    crate::data::group_picture::privatize_tx(&tx, conv)?;
    if let Some(pending) = crate::data::app_prefs::get_tx(&tx, &format!("group_pending:{}", hex::encode(conv))) {
        crate::data::app_prefs::set_tx(&tx, &format!("group_pending:{}", hex::encode(conv)), &seal_tx(&tx, conv, &pending)?)?;
    }
    let reactions = crate::db::all(
        &tx,
        "SELECT * FROM reactions WHERE conversation_id=?1",
        [conv],
        crate::db::messages::ReactionRow::from_row,
    )?;
    for r in reactions {
        crate::data::reaction::private_apply_tx(
            &tx,
            conv,
            &r.dispatch_id,
            &r.reactor,
            &r.emoji,
            true,
            r.timestamp,
        )?;
    }
    tx.execute("DELETE FROM reactions WHERE conversation_id=?1", [conv])?;
    tx.execute("UPDATE conversations SET muted=1,pinned=0 WHERE id=?1", [conv])?;
    tx.commit()?;
    crate::private_files::cleanup_plaintext_tx(&conn)?;
    // secure_delete clears freed cells; checkpoint + VACUUM rewrites old live database pages.
    conn.execute_batch(
        "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
    )?;
    drop(conn);
    crate::data::peer_avatar::notify_changed();
    if let Some(gid) = group {
        crate::mls::privacy::install(
            &gid,
            conv,
            pair.public_key().as_slice(),
            &salt,
            &[nonce.as_slice(), &wrapped].concat(),
        )?;
    }
    Ok(())
}
pub(crate) fn unlock(code: &str) -> Result<Option<[u8; 16]>> {
    let generation = core().private_generation.load(std::sync::atomic::Ordering::SeqCst);
    repair_transport()?;
    find_code(code, true, generation)
}
fn find_code(code: &str, install: bool, generation: u64) -> Result<Option<[u8; 16]>> {
    if code.len() > 256 || code.chars().count() < 6 {
        return Ok(None);
    }
    let rows: Vec<([u8; 16], Vec<u8>, Vec<u8>)> = crate::db::all(
        &core().db.messages().lock(),
        "SELECT conversation_id,salt,wrapped_key FROM private_conversations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    for (conv, salt, wrapped) in rows {
        if wrapped.len() < 40 {
            return Err(anyhow!("invalid private key record"));
        }
        let key = derive(code, &salt)?;
        if let Ok(private) = XChaCha20Poly1305::new((&*key).into()).decrypt(
            XNonce::from_slice(&wrapped[..24]),
            Payload { msg: &wrapped[24..], aad: &conv },
        ) {
            let private = Zeroizing::new(private);
            if !install {
                return Ok(Some(conv));
            }
            let mut sessions = core().private_sessions.write();
            ensure!(
                generation == core().private_generation.load(std::sync::atomic::Ordering::SeqCst),
                "Unlock cancelled"
            );
            sessions.insert(conv, private);
            return Ok(Some(conv));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::privacy::*,
        data::{message::Message, reaction::Reaction},
    };
    #[tokio::test]
    async fn history_arriving_while_locked_requires_code_and_reactions_survive() {
        let scope = crate::test_support::ScopedCore::new();
        let conv = [81; 16];
        scope
            .core
            .db
            .messages()
            .lock()
            .execute("INSERT INTO conversations(id) VALUES(?1)", [conv])
            .unwrap();
        Message::save_incoming(conv, [82; 32], &[1; 16], "old secret", 1, None).unwrap();
        Reaction::apply(&conv, &[1; 16], &[82; 32], "heart", true, 2);
        hide(&conv, "a secret code").unwrap();
        assert!(crate::api::messaging::get_messages(conv.to_vec(), 100, "".into()).is_err());
        let raw: String = scope
            .core
            .db
            .messages()
            .lock()
            .query_row("SELECT content FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(!raw.contains("old secret"));
        Message::save_incoming(conv, [82; 32], &[2; 16], "new secret", 3, None).unwrap();
        assert_eq!(unlock("wrong code").unwrap(), None);
        assert_eq!(unlock("a secret code").unwrap(), Some(conv));
        let rows = Message::get_messages(&conv, 100, "");
        assert_eq!(
            rows.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(),
            ["old secret", "new secret"]
        );
        assert_eq!(Reaction::for_conversation(&conv)[0].emoji, "heart");
        lock_private_conversations();
        Reaction::apply(&conv, &[1; 16], &[82; 32], "heart", false, 4);
        assert!(Reaction::for_conversation(&conv).is_empty());
        assert!(Message::unread_counts().is_empty());
        assert!(Message::pending_notification_ids(&conv).unwrap().is_empty());
        unlock("a secret code").unwrap();
        assert!(Reaction::for_conversation(&conv).is_empty());
        assert!(Message::dump_all_tx(&scope.core.db.messages().lock()).unwrap().is_empty());
        let other = [91; 16];
        Message::save_incoming(
            other,
            [82; 32],
            &[9; 16],
            "pz-private-v1:literal ordinary text",
            5,
            None,
        )
        .unwrap();
        assert_eq!(
            Message::get_messages(&other, 100, "")[0].content,
            "pz-private-v1:literal ordinary text"
        );
        reset_private_conversation(conv.to_vec()).unwrap();
        assert!(!is_private(&conv));
        assert!(Message::get_messages(&conv, 100, "").is_empty());
        assert!(crate::data::conversation::Conversation::get(&conv).is_some());
    }

    #[tokio::test]
    async fn stale_unlock_and_duplicate_code_never_install_a_capability() {
        let scope = crate::test_support::ScopedCore::new();
        let first = [71u8; 16];
        let second = [72u8; 16];
        for conv in [first, second] {
            scope
                .core
                .db
                .messages()
                .lock()
                .execute("INSERT INTO conversations(id) VALUES(?1)", [conv])
                .unwrap();
        }
        hide(&first, "unique-code").unwrap();
        assert!(hide(&second, "unique-code").is_err());
        assert!(core().private_sessions.read().is_empty());
        assert!(!is_private(&second));
        let old = core().private_generation.load(std::sync::atomic::Ordering::SeqCst);
        lock_private_conversations();
        assert!(find_code("unique-code", true, old).is_err());
        assert!(core().private_sessions.read().is_empty());
    }

    #[tokio::test]
    async fn resetting_an_ordinary_chat_cannot_discard_its_transport() {
        let scope = crate::test_support::ScopedCore::new();
        let conv = [73u8; 16];
        let gid = [74u8; 32];
        scope
            .core
            .db
            .messages()
            .lock()
            .execute(
                "INSERT INTO conversations(id,mls_group_id) VALUES(?1,?2)",
                rusqlite::params![conv, gid],
            )
            .unwrap();
        assert!(reset_private_conversation(conv.to_vec()).is_err());
        assert_eq!(crate::data::conversation::Conversation::group_of(&conv), Some(gid));
    }

    #[tokio::test]
    async fn transport_repair_serializes_its_snapshot_with_reset() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let scope = crate::test_support::ScopedCore::new();
        let state = scope.core;
        let (conv, gid) = ([75u8;16], [76u8;32]);
        state.db.messages().lock().execute(
            "INSERT INTO conversations(id,mls_group_id) VALUES(?1,?2)",
            rusqlite::params![conv,gid],
        ).unwrap();
        hide(&conv,"repair-race-code").unwrap();
        // Hold the operation after the repair snapshots metadata. Its configuration mutex
        // must remain held across that wait, otherwise reset could erase the code first.
        let operation = crate::mls::recovery::operation_lock(&gid).lock();
        let (started, ready) = mpsc::channel();
        let repair = std::thread::spawn(move || {
            crate::state::SCOPED.set(Some(state));
            started.send(()).unwrap();
            repair_transport()
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now()+Duration::from_secs(1);
        let serialized = loop {
            if state.private_configuration.try_lock().is_none() { break true; }
            if Instant::now()>=deadline { break false; }
            std::thread::yield_now();
        };
        let reset = std::thread::spawn(move || {
            crate::state::SCOPED.set(Some(state));
            reset_private_conversation(conv.to_vec())
        });
        drop(operation);
        repair.join().unwrap().unwrap();
        reset.join().unwrap().unwrap();
        assert!(serialized,"repair must own configuration before taking its private snapshot");
        assert!(!is_private(&conv));
        assert_eq!(state.db.mls().lock().query_row(
            "SELECT count(*) FROM mls_private_conversations WHERE conversation_id=?1",
            [conv],|r|r.get::<_,u32>(0),
        ).unwrap(),0,"a stale repair must not reinstall the discarded code mirror");
    }

    // Resetting must never reinterpret a former group's ciphertext as ordinary state or
    // lose the public identity required for authenticated membership recovery.
    #[tokio::test]
    async fn hidden_group_reset_retains_membership_identity_and_discards_every_old_key() {
        let scope = crate::test_support::ScopedCore::new();
        let signer = crate::test_support::data::identity(&scope.core.db.identity().lock(), 87);
        let me = signer.verifying_key().to_bytes();
        let peer = [88u8; 32];
        let gid = [89u8; 32];
        let conv = crate::data::conversation::Conversation::create_group("Private group", &[peer]).unwrap();
        crate::data::conversation::Conversation::bind_group(&conv, &gid).unwrap();
        crate::groups::member_requests::anchor(&gid, [90; 32]).unwrap();
        let mut key = Vec::new();
        ciborium::ser::into_writer(&openmls::prelude::GroupId::from_slice(&gid), &mut key).unwrap();
        let mls = scope.core.db.mls();
        mls.lock().execute("INSERT INTO mls_storage VALUES(?1,10,X'',?2)", rusqlite::params![key,b"old group secret".as_slice()]).unwrap();
        Message::save_incoming(conv, peer, &[5; 16], "group history", 1, None).unwrap();
        hide_conversation(conv.to_vec(), "group-reset-code".into()).unwrap();
        assert!(crate::api::messaging::list_conversations().iter().all(|c| c.id != conv));
        assert!(crate::api::messaging::get_messages(conv.to_vec(), 100, "".into()).is_err());
        assert_eq!(unlock("group-reset-code").unwrap(), Some(conv));
        assert_eq!(Message::get_messages(&conv, 100, "")[0].content, "group history");
        let backup = crate::data::private_backup::dump(&scope.core.db.messages().lock()).unwrap();
        let options=locked_conversation_reset_options().unwrap();
        assert_eq!(options.len(),1);
        assert!(options[0].is_group);
        assert_eq!(options[0].name,"Private group");
        lock_private_conversations();
        reset_private_conversation(conv.to_vec()).unwrap();
        assert_eq!(crate::data::conversation::Conversation::group_of(&conv), Some(gid));
        assert!(crate::data::conversation::Conversation::active_members(&conv).contains(&me));
        assert!(crate::data::conversation::Conversation::active_members(&conv).contains(&peer));
        assert_eq!(crate::groups::member_requests::known_anchor(&gid), Some([90;32]));
        assert!(crate::groups::member_requests::refresh_request(&gid).is_some());
        assert!(Message::get_messages(&conv, 100, "").is_empty());
        assert_eq!(mls.lock().query_row("SELECT count(*) FROM mls_storage WHERE group_id=?1", [&key], |r|r.get::<_,u32>(0)).unwrap(),0);
        assert_eq!(mls.lock().query_row("SELECT count(*) FROM mls_private_conversations WHERE group_id=?1", [gid], |r|r.get::<_,u32>(0)).unwrap(),0);
        assert!(unlock("group-reset-code").unwrap().is_none());
        // Importing an old account backup must not restore the discarded code/history on top
        // of the surviving group identity, including after the reset device has restarted.
        crate::data::private_backup::restore(&scope.core.db.messages().lock(),&backup).unwrap();
        assert!(!is_private(&conv));
        assert!(Message::get_messages(&conv,100,"").is_empty());
        assert!(unlock("group-reset-code").unwrap().is_none());
    }

    #[tokio::test]
    async fn conversion_rolls_back_when_media_cannot_be_protected() {
        let scope = crate::test_support::ScopedCore::new();
        let conv = [83; 16];
        let conn = scope.core.db.messages().lock();
        conn.execute("INSERT INTO conversations(id) VALUES(?1)", [conv]).unwrap();
        drop(conn);
        Message::save_incoming(conv, [84; 32], &[3; 16], "keep readable", 1, None).unwrap();
        scope.core.db.messages().lock().execute("INSERT INTO message_media(conversation_id,dispatch_id,kind,mime,name,file_id) VALUES(?1,?2,2,'text/plain','secret.txt',?3)",rusqlite::params![conv,[3u8;16],[4u8;32]]).unwrap();
        scope.core.db.messages().lock().execute_batch("CREATE TRIGGER refuse_media_conversion BEFORE INSERT ON message_media BEGIN SELECT RAISE(ABORT,'injected media storage failure'); END").unwrap();
        assert!(hide(&conv, "a secret code").is_err());
        assert!(!is_private(&conv));
        assert_eq!(Message::get_messages(&conv, 100, "")[0].content, "keep readable");
    }
}

/// Finishes an interrupted conversion before networking or an unlock can expose a private chat.
/// Public-key sealing needs no code. The wrapped private key is durable in messages.db first.
pub(crate) fn repair_transport() -> Result<()> {
    // Reset deletes the mirrored transport and the code record under this mutex. Taking the
    // same configuration→operation order keeps a stale repair from reinstalling that mirror.
    let _configuration = core().private_configuration.lock();
    let rows: Vec<([u8; 16], Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> = crate::db::all(
        &core().db.messages().lock(),
        "SELECT p.conversation_id,c.mls_group_id,p.public_key,p.salt,p.wrapped_key FROM private_conversations p JOIN conversations c ON c.id=p.conversation_id WHERE c.mls_group_id IS NOT NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    for (conv, gid, public, salt, wrapped) in rows {
        let gid: [u8; 32] = gid.try_into().map_err(|_| anyhow!("invalid private group id"))?;
        let _operation = crate::mls::recovery::operation_lock(&gid).lock();
        crate::mls::privacy::install(&gid, &conv, &public, &salt, &wrapped)?;
    }
    Ok(())
}
