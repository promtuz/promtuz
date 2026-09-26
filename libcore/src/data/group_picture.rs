//! Versioned, admin-owned group photos.
pub(crate) fn snapshot(conv: &[u8; 16]) -> Option<(u64, Option<Vec<u8>>)> {
    crate::db::messages::MESSAGES_DB
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
    let db = crate::db::messages::MESSAGES_DB.lock();
    apply(&db, &conv, &author, revision, avif)?;
    drop(db);
    crate::data::peer_avatar::notify_changed();
    Ok(())
}

fn apply(
    db: &rusqlite::Connection, conv: &[u8; 16], author: &[u8; 32], revision: u64,
    avif: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    use crate::data::conversation::{Conversation, KIND_GROUP};
    let kind: u8 =
        db.query_row("SELECT kind FROM conversations WHERE id=?1", [conv.as_slice()], |r| {
            r.get(0)
        })?;
    anyhow::ensure!(kind == KIND_GROUP, "not a group");
    if let Some(bytes) = &avif {
        crate::data::peer_avatar::check_avif(bytes)?;
    }
    let revision = i64::try_from(revision)?;
    anyhow::ensure!(
        Conversation::is_admin_tx(db, conv, author),
        "only the active admin can change the picture"
    );
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

pub fn dump() -> Vec<Backup> {
    let db = crate::db::messages::MESSAGES_DB.lock();
    db.prepare("SELECT conversation_id, revision, avif FROM group_pictures")
        .and_then(|mut q| {
            q.query_map([], |r| {
                Ok(Backup { conversation: r.get(0)?, revision: r.get(1)?, avif: r.get(2)? })
            })
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

pub fn restore(rows: &[Backup]) -> anyhow::Result<()> {
    let db = crate::db::messages::MESSAGES_DB.lock();
    let tx = db.unchecked_transaction()?;
    for r in rows {
        if let Some(bytes) = &r.avif {
            crate::data::peer_avatar::check_avif(bytes)?;
        }
        let revision = i64::try_from(r.revision)?;
        // A current device's copy wins over an imported snapshot, including removals.
        tx.execute(
            "INSERT OR IGNORE INTO group_pictures(conversation_id,revision,avif) VALUES (?1,?2,?3)",
            (r.conversation.as_slice(), revision, &r.avif),
        )?;
    }
    tx.commit()?;
    drop(db);
    crate::data::peer_avatar::notify_changed();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_active_admin_can_update_and_old_photos_cannot_undo_removal() {
        let db = crate::db::messages::open_in_memory();
        let conv = [1u8; 16];
        let admin = [2u8; 32];
        let member = [3u8; 32];
        db.execute("INSERT INTO conversations(id,kind) VALUES (?1,1)", [conv.as_slice()]).unwrap();
        for (peer, role) in [(admin, 1), (member, 0)] {
            db.execute("INSERT INTO conversation_members(conversation_id,member_ipk,role) VALUES (?1,?2,?3)",
                (conv.as_slice(),peer.as_slice(),role)).unwrap();
        }
        let photo = Some(vec![
            0, 0, 0, 24, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f', 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ]);
        apply(&db, &conv, &admin, 1, photo.clone()).unwrap();
        assert!(apply(&db, &conv, &member, 3, None).is_err());
        apply(&db, &conv, &admin, 2, None).unwrap();
        apply(&db, &conv, &admin, 1, photo).unwrap();
        let kept: (u64, Option<Vec<u8>>) = db
            .query_row("SELECT revision,avif FROM group_pictures", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(kept, (2, None));
        db.execute(
            "UPDATE conversation_members SET active=0 WHERE member_ipk=?1",
            [admin.as_slice()],
        )
        .unwrap();
        assert!(apply(&db, &conv, &admin, 4, None).is_err());
    }
}
