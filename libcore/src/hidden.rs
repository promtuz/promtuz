//! Hidden chats. A hidden chat is a locked 1:1 chat whose person is sealed into its vault while it
//! is locked: contact, roster, name, avatar, profile and receipts. Unlocking restores those rows so
//! the rest of the app works unchanged; locking seals them again.
use crate::data::private_backup::{Table, dump_where, insert};
use crate::state::core;
use anyhow::{Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub(crate) const COLORS: u8 = 8;

/// Rows tied to the chat (`?1` is the conversation) or to the person (`?1` is their key).
const CHAT_ROWS: &[(&str, &str)] = &[
    ("member_read_state", "conversation_id=?1"),
    ("receipt_peers", "conversation_id=?1"),
    ("message_deletions", "conversation_id=?1"),
    ("attachment_sharing", "conversation_id=?1"),
    ("attachment_sharing_revocations", "conversation_id=?1"),
    ("message_recipients", "message_id IN (SELECT id FROM messages WHERE conversation_id=?1)"),
    // Last: the person is found through the roster until every other row is gone.
    ("conversation_members", "conversation_id=?1"),
];
/// Kept while a group we share still shows them, as it would for any other member.
const PROFILE_ROWS: &[(&str, &str)] = &[
    ("peer_names", "ipk=?1"),
    ("peer_avatars", "ipk=?1"),
    ("peer_profiles", "ipk=?1"),
    ("profile_fields", "owner=?1"),
];
const NETWORK_ROWS: &[(&str, &str)] = &[("presence_contacts", "peer=?1"), ("presence_interest", "peer=?1")];

#[derive(Serialize, Deserialize)]
struct Person {
    ipk: [u8; 32],
    title: String,
    senders: Vec<String>,
    contacts: Vec<Table>,
    network: Vec<Table>,
    messages: Vec<Table>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub code: String,
    pub color: u8,
}
impl Tag {
    fn encode(&self) -> String {
        format!("{}:{}", self.code, self.color)
    }
    fn decode(s: &str) -> Option<Self> {
        let (code, color) = s.split_once(':')?;
        Some(Self { code: code.into(), color: color.parse().ok()? })
    }
}

fn derive(context: &str, shared: &[u8; 32], extra: &[&[u8]]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new_derive_key(context);
    hash.update(shared);
    for part in extra {
        hash.update(part);
    }
    *hash.finalize().as_bytes()
}
/// Recognizes the person without naming them. Only our identity secret can compute it.
pub(crate) fn marker_of(peer: &[u8; 32]) -> Result<[u8; 32]> {
    Ok(derive("promtuz 2026-11 hidden chat marker", &*crate::data::identity::shared_with(peer)?, &[]))
}
/// Two characters and a color, as the design shows. Deterministic, so the other person can
/// compute the same tag; a counter breaks collisions among our own.
fn tag_of(peer: &[u8; 32], conv: &[u8; 16], counter: u8) -> Result<Tag> {
    let h = derive("promtuz 2026-11 hidden chat tag", &*crate::data::identity::shared_with(peer)?, &[conv, &[counter]]);
    let bits = u16::from_be_bytes([h[0], h[1]]);
    let code = [5, 0].iter().map(|s| ALPHABET[((bits >> s) & 31) as usize] as char).collect();
    Ok(Tag { code, color: h[2] % COLORS })
}

pub(crate) fn is_hidden_tx(conn: &Connection, conv: &[u8; 16]) -> Result<bool> {
    Ok(conn
        .query_row("SELECT hidden FROM private_conversations WHERE conversation_id=?1", [conv], |r| r.get(0))
        .optional()?
        .unwrap_or(false))
}
pub(crate) fn is_hidden(conv: &[u8; 16]) -> bool {
    is_hidden_tx(&core().db.messages().lock(), conv).unwrap_or(false)
}
/// Hidden and locked: nothing outside the vault may mention the chat.
pub(crate) fn is_concealed(conv: &[u8; 16]) -> bool {
    is_hidden(conv) && !core().private_sessions.read().contains_key(conv)
}
pub(crate) fn tag(conv: &[u8; 16]) -> Option<Tag> {
    let conn = core().db.messages().lock();
    let tag: Option<String> = conn
        .query_row("SELECT tag FROM private_conversations WHERE conversation_id=?1 AND hidden=1", [conv], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
        .flatten();
    tag.as_deref().and_then(Tag::decode)
}

/// The marker standing in for `sender` while their hidden chat is sealed.
pub(crate) fn concealed_sender(sender: &[u8; 32]) -> Option<([u8; 32], [u8; 16])> {
    let any: bool = core()
        .db
        .messages()
        .lock()
        .query_row("SELECT EXISTS(SELECT 1 FROM private_conversations WHERE hidden=1 AND vault IS NOT NULL)", [], |r| r.get(0))
        .ok()?;
    if !any {
        return None;
    }
    let marker = marker_of(sender).ok()?;
    let conv: [u8; 16] = core()
        .db
        .messages()
        .lock()
        .query_row(
            "SELECT conversation_id FROM private_conversations WHERE hidden=1 AND vault IS NOT NULL AND marker=?1",
            [marker],
            |r| r.get(0),
        )
        .optional()
        .ok()??;
    Some((marker, conv))
}
/// Our hidden chat with `peer`, sealed or open.
pub(crate) fn chat_with(peer: &[u8; 32]) -> Option<[u8; 16]> {
    let any: bool = core()
        .db
        .messages()
        .lock()
        .query_row("SELECT EXISTS(SELECT 1 FROM private_conversations WHERE hidden=1)", [], |r| r.get(0))
        .ok()?;
    if !any {
        return None;
    }
    let marker = marker_of(peer).ok()?;
    core()
        .db
        .messages()
        .lock()
        .query_row("SELECT conversation_id FROM private_conversations WHERE hidden=1 AND marker=?1", [marker], |r| r.get(0))
        .optional()
        .ok()?
}

/// The chat must already be locked. An open session stays open; the person is sealed on lock.
pub(crate) fn hide(conv: &[u8; 16]) -> Result<Tag> {
    let _configuration = core().private_configuration.lock();
    let peer = {
        let me = crate::data::identity::Identity::local_ipk();
        let conn = core().db.messages().lock();
        let direct = crate::data::conversation::Conversation::get_tx(&conn, conv)
            .is_some_and(|c| c.kind == crate::data::conversation::KIND_DIRECT);
        ensure!(direct, "Only a chat with one person can be hidden");
        ensure!(crate::privacy::is_private_tx(&conn, conv)?, "Lock the chat first");
        ensure!(!is_hidden_tx(&conn, conv)?, "Chat is already hidden");
        crate::data::conversation::Conversation::peer_of_tx(&conn, conv, me).ok_or_else(|| anyhow!("Chat not found"))?
    };
    let marker = marker_of(&peer)?;
    let taken: Vec<String> = crate::db::all(
        &core().db.messages().lock(),
        "SELECT tag FROM private_conversations WHERE hidden=1 AND tag IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let tag = (0..=u8::MAX)
        .map(|n| tag_of(&peer, conv, n))
        .find(|t| t.as_ref().map_or(true, |t| !taken.iter().any(|s| Tag::decode(s).is_some_and(|o| o.code == t.code))))
        .ok_or_else(|| anyhow!("no free tag"))??;
    core().db.messages().lock().execute(
        "UPDATE private_conversations SET hidden=1,marker=?2,tag=?3,vault=NULL WHERE conversation_id=?1",
        params![conv, marker, tag.encode()],
    )?;
    seal_held(conv)?;
    Ok(tag)
}

/// Back to an ordinary locked chat. The person's rows are already restored by the open session.
pub(crate) fn unhide(conv: &[u8; 16]) -> Result<()> {
    let _configuration = core().private_configuration.lock();
    ensure!(core().private_sessions.read().contains_key(conv), "Chat is locked");
    let conn = core().db.messages().lock();
    ensure!(is_hidden_tx(&conn, conv)?, "Chat is not hidden");
    ensure!(
        conn.query_row("SELECT vault IS NULL FROM private_conversations WHERE conversation_id=?1", [conv], |r| r.get::<_, bool>(0))?,
        "Chat is locked"
    );
    conn.execute(
        "UPDATE private_conversations SET hidden=0,marker=NULL,tag=NULL,vault=NULL WHERE conversation_id=?1",
        [conv],
    )?;
    Ok(())
}

fn shares_a_group(conn: &Connection, conv: &[u8; 16], peer: &[u8; 32]) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_members m JOIN conversations c ON c.id=m.conversation_id
            WHERE m.member_ipk=?1 AND c.id<>?2 AND c.kind=?3)",
        params![peer, conv, crate::data::conversation::KIND_GROUP],
        |r| r.get(0),
    )?)
}

/// Seals the person of a hidden chat that is not open. Idempotent and safe to resume: the vault is
/// written first, then rows are removed with the roster last, so the person stays findable until
/// the end. A vault already present is authoritative and is never rebuilt.
pub(crate) fn seal(conv: &[u8; 16]) -> Result<()> {
    let _configuration = core().private_configuration.lock();
    seal_held(conv)
}
/// `seal` for a caller already holding the private configuration lock.
pub(crate) fn seal_held(conv: &[u8; 16]) -> Result<()> {
    if core().private_sessions.read().contains_key(conv) {
        return Ok(());
    }
    let me = crate::data::identity::Identity::local_ipk();
    let (peer, vault, public) = {
        let conn = core().db.messages().lock();
        let row: Option<(bool, Option<Vec<u8>>, Vec<u8>)> = conn
            .query_row(
                "SELECT hidden,vault,public_key FROM private_conversations WHERE conversation_id=?1",
                [conv],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((true, vault, public)) = row else { return Ok(()) };
        let Some(peer) = crate::data::conversation::Conversation::peer_of_tx(&conn, conv, me) else {
            return Ok(());
        };
        (peer, vault, public)
    };
    let marker = marker_of(&peer)?;
    let profile = !shares_a_group(&core().db.messages().lock(), conv, &peer)?;
    if vault.is_none() {
        let person = {
            let contacts = vec![dump_where(&core().db.contacts().lock(), "contacts", "ipk=?1", &peer)?];
            let network = NETWORK_ROWS
                .iter()
                .map(|(t, f)| dump_where(&core().db.network().lock(), t, f, &peer))
                .collect::<Result<Vec<_>>>()?;
            let conn = core().db.messages().lock();
            let mut messages = Vec::new();
            for (t, f) in CHAT_ROWS {
                messages.push(dump_where(&conn, t, f, conv)?);
            }
            if profile {
                for (t, f) in PROFILE_ROWS {
                    messages.push(dump_where(&conn, t, f, &peer)?);
                }
            }
            Person {
                ipk: peer,
                title: conn.query_row("SELECT title FROM conversations WHERE id=?1", [conv], |r| r.get(0))?,
                senders: crate::db::all(
                    &conn,
                    "SELECT id FROM messages WHERE conversation_id=?1 AND sender_ipk=?2",
                    params![conv, peer],
                    |r| r.get(0),
                )?,
                contacts,
                network,
                messages,
            }
        };
        let sealed = crate::privacy::seal_with_public(conv, &public, &zeroize::Zeroizing::new(postcard::to_allocvec(&person)?))?;
        core().db.messages().lock().execute(
            "UPDATE private_conversations SET vault=?2 WHERE conversation_id=?1 AND hidden=1",
            params![conv, sealed],
        )?;
    }
    if let Some(gid) = crate::data::conversation::Conversation::group_of(conv) {
        let mls = core().db.mls();
        let mut conn = mls.lock();
        let tx = conn.transaction()?;
        let buffered: Vec<(i64, [u8; 32], Vec<u8>)> = crate::db::all(
            &tx,
            "SELECT rowid,sender,envelope FROM mls_private_inbox WHERE group_id=?1 AND hidden=0",
            [gid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        for (row, sender, envelope) in buffered {
            let wrapped = crate::privacy::seal_with_public(conv, &public, &zeroize::Zeroizing::new(postcard::to_allocvec(&(sender, envelope))?))?;
            tx.execute(
                "UPDATE OR IGNORE mls_private_inbox SET sender=?2,envelope=?3,hidden=1 WHERE rowid=?1",
                params![row, marker, wrapped],
            )?;
        }
        tx.commit()?;
    }
    core().db.contacts().lock().execute("DELETE FROM contacts WHERE ipk=?1", [peer])?;
    // Pending deliveries still need the key; dead ones are never retried.
    core().db.outbox().lock().execute("DELETE FROM outbox WHERE target_ipk=?1 AND state=1", [peer])?;
    {
        let conn = core().db.network().lock();
        for (t, f) in NETWORK_ROWS {
            conn.execute(&format!("DELETE FROM {t} WHERE {f}"), [peer])?;
        }
    }
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE OR IGNORE seen_dispatch SET sender_ipk=?2 WHERE sender_ipk=?1",
        params![peer, marker],
    )?;
    tx.execute("DELETE FROM seen_dispatch WHERE sender_ipk=?1", [peer])?;
    tx.execute("UPDATE messages SET sender_ipk=NULL WHERE conversation_id=?1 AND sender_ipk=?2", params![conv, peer])?;
    tx.execute("UPDATE conversations SET title='' WHERE id=?1", [conv])?;
    if profile {
        for (t, f) in PROFILE_ROWS {
            tx.execute(&format!("DELETE FROM {t} WHERE {f}"), [peer])?;
        }
    }
    for (t, f) in CHAT_ROWS {
        tx.execute(&format!("DELETE FROM {t} WHERE {f}"), [conv])?;
    }
    tx.commit()?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    drop(conn);
    for db in [core().db.contacts(), core().db.network(), core().db.outbox()] {
        let _ = db.lock().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
    crate::data::peer_avatar::notify_changed();
    Ok(())
}

/// Restores the person for an open session. The vault is cleared last, so an interrupted restore
/// leaves it authoritative and the next `seal` finishes removing whatever was restored.
pub(crate) fn open(conv: &[u8; 16]) -> Result<()> {
    let _configuration = core().private_configuration.lock();
    // A lock since the session began has already sealed the person again.
    if !core().private_sessions.read().contains_key(conv) {
        return Ok(());
    }
    let vault: Option<Vec<u8>> = core()
        .db
        .messages()
        .lock()
        .query_row("SELECT vault FROM private_conversations WHERE conversation_id=?1 AND hidden=1", [conv], |r| r.get(0))
        .optional()?
        .flatten();
    let Some(vault) = vault else { return Ok(()) };
    let person: Person = postcard::from_bytes(&zeroize::Zeroizing::new(crate::privacy::open_bytes(conv, &vault)?))?;
    let marker = marker_of(&person.ipk)?;
    for table in &person.contacts {
        insert(&core().db.contacts().lock(), table)?;
    }
    for table in &person.network {
        insert(&core().db.network().lock(), table)?;
    }
    let mut conn = core().db.messages().lock();
    let tx = conn.transaction()?;
    for table in &person.messages {
        insert(&tx, table)?;
    }
    for id in &person.senders {
        tx.execute("UPDATE messages SET sender_ipk=?2 WHERE id=?1", params![id, person.ipk])?;
    }
    tx.execute("UPDATE conversations SET title=?2 WHERE id=?1 AND title=''", params![conv, person.title])?;
    tx.execute(
        "UPDATE OR IGNORE seen_dispatch SET sender_ipk=?2 WHERE sender_ipk=?1",
        params![marker, person.ipk],
    )?;
    tx.execute("UPDATE private_conversations SET vault=NULL WHERE conversation_id=?1", [conv])?;
    tx.commit()?;
    drop(conn);
    crate::data::peer_avatar::notify_changed();
    Ok(())
}

/// Seals every hidden chat whose session is closed: on lock, and at startup after a crash.
pub(crate) fn seal_all() {
    let ids: Vec<[u8; 16]> = crate::db::all(
        &core().db.messages().lock(),
        "SELECT conversation_id FROM private_conversations WHERE hidden=1",
        [],
        |r| r.get(0),
    )
    .unwrap_or_default();
    for conv in ids {
        if let Err(error) = seal(&conv) {
            log::warn!("HIDDEN: a chat stays unsealed until the next lock: {error:#}");
        }
    }
}

/// Seals every hidden chat for the duration of `f`, then reopens the ones that were open. A lock
/// during `f` closes them for good.
pub(crate) fn sealed_during<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    let unsealed: Vec<[u8; 16]> = crate::db::all(
        &core().db.messages().lock(),
        "SELECT conversation_id FROM private_conversations WHERE hidden=1 AND vault IS NULL",
        [],
        |r| r.get(0),
    )?;
    let (generation, saved) = {
        let mut sessions = core().private_sessions.write();
        let saved: Vec<_> = unsealed.iter().filter_map(|c| sessions.remove(c).map(|key| (*c, key))).collect();
        (core().private_generation.load(std::sync::atomic::Ordering::SeqCst), saved)
    };
    let result = (|| {
        for conv in &unsealed {
            seal(conv)?;
        }
        f()
    })();
    let reopened: Vec<[u8; 16]> = {
        let mut sessions = core().private_sessions.write();
        if generation != core().private_generation.load(std::sync::atomic::Ordering::SeqCst) {
            Vec::new()
        } else {
            saved.into_iter().map(|(conv, key)| { sessions.insert(conv, key); conv }).collect()
        }
    };
    for conv in reopened {
        if let Err(error) = open(&conv) {
            log::warn!("HIDDEN: could not restore an open chat: {error:#}");
        }
        // Messages that arrived while it was sealed waited in the private inbox.
        core().spawn(async move {
            if let Err(error) = crate::mls::privacy::drain(conv).await {
                log::warn!("PRIVATE: buffered inbox remains pending: {error:#}");
            }
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use crate::api::privacy::*;
    use crate::data::{contact::Contact, conversation::Conversation, message::Message, seen::Seen};
    use rusqlite::Connection;

    /// Every row in `conn` holding `peer` in any column.
    fn mentions(conn: &Connection, peer: &[u8; 32]) -> Vec<String> {
        let tables: Vec<String> = crate::db::all(conn, "SELECT name FROM sqlite_master WHERE type='table'", [], |r| r.get(0)).unwrap();
        let mut found = Vec::new();
        for table in tables {
            let columns: Vec<String> = crate::db::all(conn, &format!("PRAGMA table_info({table})"), [], |r| r.get(1)).unwrap();
            for column in columns {
                let n: u32 = conn
                    .query_row(&format!("SELECT count(*) FROM {table} WHERE \"{column}\"=?1"), [peer], |r| r.get(0))
                    .unwrap();
                if n > 0 {
                    found.push(format!("{table}.{column}"));
                }
            }
        }
        found
    }
    fn everywhere(scope: &crate::test_support::ScopedCore, peer: &[u8; 32]) -> Vec<String> {
        let db = &scope.core.db;
        let mut found = mentions(&db.messages().lock(), peer);
        found.extend(mentions(&db.contacts().lock(), peer));
        found.extend(mentions(&db.network().lock(), peer));
        found.extend(mentions(&db.mls().lock(), peer));
        found.extend(mentions(&db.outbox().lock(), peer));
        found
    }

    fn chat(scope: &crate::test_support::ScopedCore) -> ([u8; 32], [u8; 16]) {
        crate::test_support::data::identity(&scope.core.db.identity().lock(), 1);
        let peer = ed25519_dalek::SigningKey::from_bytes(&[2; 32]).verifying_key().to_bytes();
        {
            let db = scope.core.db.contacts().lock();
            Contact::save_pending_tx(&db, peer, "Lennox", 0).unwrap();
            Contact::mark_paired_tx(&db, &peer).unwrap();
        }
        let conv = Conversation::for_peer(&peer).unwrap();
        let db = scope.core.db.messages();
        db.lock().execute("UPDATE conversations SET mls_group_id=?2,title='Len' WHERE id=?1", rusqlite::params![conv, [3u8; 32]]).unwrap();
        db.lock().execute("INSERT INTO peer_names(ipk,name,updated_at) VALUES(?1,'Lennox',0)", [peer]).unwrap();
        scope.core.db.network().lock().execute("INSERT INTO presence_interest(peer) VALUES(?1)", [peer]).unwrap();
        Message::save_incoming(conv, peer, &[1; 16], "hello", 1, None).unwrap();
        Seen::record(&peer, &[1; 16], 1);
        scope.core.db.outbox().lock().execute(
            "INSERT INTO outbox(id,op_type,target_ipk,payload,created_at,state) VALUES(X'01',0,?1,X'',0,1)", [peer]).unwrap();
        (peer, conv)
    }

    #[tokio::test]
    async fn a_hidden_person_leaves_no_row_while_locked_and_returns_with_the_code() {
        let scope = crate::test_support::ScopedCore::new();
        let (peer, conv) = chat(&scope);
        let before = everywhere(&scope, &peer);
        for row in ["contacts.ipk", "conversation_members.member_ipk", "peer_names.ipk", "presence_interest.peer", "seen_dispatch.sender_ipk", "messages.sender_ipk"] {
            assert!(before.iter().any(|r| r == row), "{row} in {before:?}");
        }
        lock_conversation(conv.to_vec(), "hidden-code".into()).unwrap();
        assert!(hide_conversation(conv.to_vec(), None).is_err(), "hiding a locked chat needs its session");
        assert_eq!(crate::privacy::unlock("hidden-code").unwrap(), Some(conv));
        let tag = hide_conversation(conv.to_vec(), None).unwrap();
        assert_eq!(tag.code.len(), 2);
        assert!(Contact::exists(&peer), "the open chat stays usable until it locks");
        lock_private_conversations();

        assert_eq!(everywhere(&scope, &peer), Vec::<String>::new());
        assert!(!crate::api::messaging::list_conversations().iter().any(|c| c.id == conv));
        assert!(!crate::api::messaging::get_conversations().iter().any(|m| m.conversation_id == conv));
        assert!(super::concealed_sender(&peer).is_some_and(|(_, c)| c == conv));
        assert!(Seen::contains(&peer, &[1; 16]), "deduplication survives sealing");
        Seen::record(&peer, &[2; 16], 2);
        assert_eq!(everywhere(&scope, &peer), Vec::<String>::new());
        assert!(!Contact::exists(&peer), "a re-add sees a stranger");
        assert_eq!(super::chat_with(&peer), Some(conv));
        assert_eq!(locked_conversation_reset_options().unwrap()[0].name, "");

        assert_eq!(crate::privacy::unlock("hidden-code").unwrap(), Some(conv));
        assert!(Contact::exists(&peer));
        assert_eq!(Conversation::peer_of(&conv), Some(peer));
        assert_eq!(Message::get_messages(&conv, 10, "")[0].sender_ipk, Some(peer.to_vec()));
        assert!(Seen::contains(&peer, &[2; 16]));
        let record = crate::api::messaging::list_conversations().into_iter().find(|c| c.id == conv).unwrap();
        assert!(record.hidden && record.locked);
        assert_eq!((record.title.as_str(), record.display_name.as_str()), ("Len", "Len"));
        assert_eq!(hidden_tag(conv.to_vec()).map(|t| t.code), Some(tag.code));

        lock_private_conversations();
        assert_eq!(everywhere(&scope, &peer), Vec::<String>::new());

        crate::privacy::unlock("hidden-code").unwrap();
        unhide_conversation(conv.to_vec()).unwrap();
        lock_private_conversations();
        assert!(Contact::exists(&peer));
        assert!(crate::api::messaging::list_conversations().iter().any(|c| c.id == conv && c.locked && !c.hidden));
        assert_eq!(super::chat_with(&peer), None);
    }

    #[tokio::test]
    async fn resetting_a_hidden_chat_forgets_it_and_frees_the_person() {
        let scope = crate::test_support::ScopedCore::new();
        let (peer, conv) = chat(&scope);
        hide_conversation(conv.to_vec(), Some("forgotten-code".into())).unwrap();
        reset_private_conversation(conv.to_vec()).unwrap();
        assert!(Conversation::get(&conv).is_none());
        assert_eq!(super::chat_with(&peer), None);
        assert!(super::concealed_sender(&peer).is_none());
        assert!(!Contact::exists(&peer));
    }

    #[tokio::test]
    async fn an_interrupted_seal_finishes_from_its_vault() {
        let scope = crate::test_support::ScopedCore::new();
        let (peer, conv) = chat(&scope);
        hide_conversation(conv.to_vec(), Some("crash-code".into())).unwrap();
        // The app died while the chat was open: its session is gone, its rows are in the clear.
        scope.core.private_sessions.write().clear();
        super::seal_all();
        assert_eq!(everywhere(&scope, &peer), Vec::<String>::new());
        crate::privacy::unlock("crash-code").unwrap();
        assert!(Contact::exists(&peer));
    }

    #[tokio::test]
    async fn a_lock_during_a_backup_keeps_the_hidden_chat_sealed() {
        let scope = crate::test_support::ScopedCore::new();
        let (peer, conv) = chat(&scope);
        hide_conversation(conv.to_vec(), Some("backup-code".into())).unwrap();
        assert!(Contact::exists(&peer));
        super::sealed_during(|| Ok(lock_private_conversations())).unwrap();
        assert!(!scope.core.private_sessions.read().contains_key(&conv));
        assert_eq!(everywhere(&scope, &peer), Vec::<String>::new());

        crate::privacy::unlock("backup-code").unwrap();
        super::sealed_during(|| Ok(())).unwrap();
        assert!(Contact::exists(&peer), "without a lock the open chat comes back");
    }
}
