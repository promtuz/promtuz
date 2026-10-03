//! Versioned, admin-owned group photos.

use crate::state::core;

pub(crate) fn snapshot(conv: &[u8; 16]) -> Option<(u64, Option<Vec<u8>>)> {
    core().db.messages()
        .lock()
        .query_row(
            "SELECT revision, avif FROM group_pictures WHERE conversation_id=?1",
            [conv.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
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
    let revision = i64::try_from(revision)?;
    db.execute(
        "INSERT INTO group_pictures(conversation_id, revision, avif) VALUES (?1, ?2, ?3)
        ON CONFLICT(conversation_id) DO UPDATE SET revision=excluded.revision, avif=excluded.avif
        WHERE excluded.revision > group_pictures.revision",
        (conv.as_slice(), revision, avif),
    )?;
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
        "SELECT conversation_id AS conversation, revision, avif FROM group_pictures",
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
