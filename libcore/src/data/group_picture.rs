//! Versioned, admin-owned group photos.

use crate::state::core;
use rusqlite::OptionalExtension;

pub(crate) fn snapshot(conv: &[u8; 16]) -> Option<(u64, Option<Vec<u8>>)> {
    let db = core().db.messages().lock();
    let (revision, stored): (u64, Option<Vec<u8>>) = db.query_row(
        "SELECT revision, avif FROM group_pictures WHERE conversation_id=?1",
        [conv.as_slice()], |r| Ok((r.get(0)?, r.get(1)?)),
    ).ok()?;
    let private = crate::privacy::is_private_tx(&db, conv).ok()?;
    if private && !core().private_sessions.read().contains_key(conv) { return None; }
    let avif = stored.map(|bytes| {
        if private { crate::privacy::open_bytes(conv, &bytes) } else { Ok(bytes) }
    }).transpose().ok()?;
    Some((revision, avif))
}

pub(crate) fn receive(
    conv: [u8; 16], author: [u8; 32], revision: u64, avif: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        crate::data::conversation::Conversation::may_edit(&conv, &author),
        "they may not change the group's photo"
    );
    receive_authorized(conv, revision, avif)
}

/// Called after MLS checked the author's permission at the message's epoch.
pub(crate) fn receive_authorized(
    conv: [u8; 16], revision: u64, avif: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    let db = core().db.messages().lock();
    store(&db, &conv, revision, avif)?;
    drop(db);
    crate::data::peer_avatar::notify_changed();
    Ok(())
}

fn store(
    db: &rusqlite::Connection, conv: &[u8; 16], revision: u64, avif: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    use crate::data::conversation::KIND_GROUP;
    let kind: u8 =
        db.query_row("SELECT kind FROM conversations WHERE id=?1", [conv.as_slice()], |r| {
            r.get(0)
        })?;
    anyhow::ensure!(kind == KIND_GROUP, "not a group");
    if let Some(bytes) = &avif {
        crate::data::peer_avatar::check_avif(bytes)?;
    }
    let avif = avif.map(|bytes| crate::privacy::seal_bytes_tx(db, conv, &bytes)).transpose()?;
    let revision = i64::try_from(revision)?;
    db.execute(
        "INSERT INTO group_pictures(conversation_id, revision, avif) VALUES (?1, ?2, ?3)
        ON CONFLICT(conversation_id) DO UPDATE SET revision=excluded.revision, avif=excluded.avif
        WHERE excluded.revision > group_pictures.revision",
        (conv.as_slice(), revision, avif),
    )?;
    Ok(())
}

/// Runs inside the history conversion transaction, so a failed photo conversion rolls back
/// the code record and every other row rather than leaving a readable recovery copy.
pub(crate) fn privatize_tx(db: &rusqlite::Connection, conv: &[u8;16]) -> anyhow::Result<()> {
    let photo:Option<Vec<u8>>=db.query_row("SELECT avif FROM group_pictures WHERE conversation_id=?1",[conv],|r|r.get::<_,Option<Vec<u8>>>(0)).optional()?.flatten();
    if let Some(photo)=photo {
        let protected=crate::privacy::seal_bytes_tx(db,conv,&photo)?;
        db.execute("UPDATE group_pictures SET avif=?2 WHERE conversation_id=?1",rusqlite::params![conv,protected])?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Backup {
    pub conversation: [u8; 16],
    pub revision: u64,
    pub avif: Option<Vec<u8>>,
}

crate::db::from_row!(Backup { conversation, revision, avif });

pub fn dump_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Backup>> {
    crate::db::all(
        conn,
        "SELECT conversation_id AS conversation, revision, avif FROM group_pictures
         WHERE conversation_id NOT IN (SELECT conversation_id FROM private_conversations)",
        [],
        Backup::from_row,
    )
}

/// A picture whose chat is missing, or that fails the gate, is skipped: it must not fail the
/// restore it rides in.
pub fn restore_tx(conn: &rusqlite::Connection, rows: &[Backup]) -> anyhow::Result<()> {
    for r in rows {
        if r.avif.as_deref().is_some_and(|b| crate::data::peer_avatar::check_avif(b).is_err()) {
            continue;
        }
        let revision = i64::try_from(r.revision)?;
        // A current device's copy wins over an imported snapshot, including removals.
        conn.execute(
            "INSERT OR IGNORE INTO group_pictures(conversation_id,revision,avif)
             SELECT ?1,?2,?3 WHERE EXISTS(SELECT 1 FROM conversations WHERE id=?1)",
            (r.conversation.as_slice(), revision, &r.avif),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod privacy_tests {
    use super::*;
    #[tokio::test]
    async fn locked_group_photo_is_code_protected_in_storage_and_backups() {
        let scope=crate::test_support::ScopedCore::new();
        let conv=crate::data::conversation::Conversation::create_group("Group", &[]).unwrap();
        let photo=b"\x00\x00\x00\x14ftypavif-private-photo".to_vec();
        receive_authorized(conv,1,Some(photo.clone())).unwrap();
        crate::privacy::lock(&conv,"photo-secret-code").unwrap();
        let raw:Vec<u8>=scope.core.db.messages().lock().query_row("SELECT avif FROM group_pictures WHERE conversation_id=?1",[conv],|r|r.get(0)).unwrap();
        assert_ne!(raw,photo);
        assert!(snapshot(&conv).is_none());
        assert!(dump_tx(&scope.core.db.messages().lock()).unwrap().is_empty());
        let backup=crate::data::private_backup::dump(&scope.core.db.messages().lock()).unwrap();
        let encoded=postcard::to_allocvec(&backup).unwrap();
        assert!(!encoded.windows(photo.len()).any(|bytes|bytes==photo));
        crate::privacy::unlock("photo-secret-code").unwrap();
        assert_eq!(snapshot(&conv),Some((1,Some(photo.clone()))));
        crate::api::privacy::lock_private_conversations();
        let restored=crate::test_support::ScopedCore::new();
        crate::data::private_backup::restore(&restored.core.db.messages().lock(),&backup).unwrap();
        assert!(snapshot(&conv).is_none());
        crate::privacy::unlock("photo-secret-code").unwrap();
        assert_eq!(snapshot(&conv),Some((1,Some(photo))));
    }
    #[tokio::test]
    async fn a_photo_conversion_failure_rolls_back_the_whole_privacy_boundary() {
        let scope=crate::test_support::ScopedCore::new();
        let conv=crate::data::conversation::Conversation::create_group("Group", &[]).unwrap();
        let photo=b"\x00\x00\x00\x14ftypavif-private-photo".to_vec();
        receive_authorized(conv,1,Some(photo.clone())).unwrap();
        crate::data::message::Message::save_incoming(conv,[151;32],&[152;16],"still ordinary",1,None).unwrap();
        scope.core.db.messages().lock().execute_batch("CREATE TRIGGER refuse_photo_conversion BEFORE UPDATE ON group_pictures BEGIN SELECT RAISE(ABORT,'injected photo storage failure'); END").unwrap();
        assert!(crate::privacy::lock(&conv,"photo-secret-code").is_err());
        assert!(!crate::privacy::is_private(&conv));
        assert_eq!(snapshot(&conv),Some((1,Some(photo))));
        assert_eq!(crate::data::message::Message::get_messages(&conv,100,"")[0].content,"still ordinary");
    }

}
