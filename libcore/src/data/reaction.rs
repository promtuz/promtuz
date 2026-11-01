//! Emoji reactions, one row per (message, reactor, emoji).

use crate::db::all;
use crate::db::messages::ReactionRow;
use crate::state::core;

pub struct Reaction;

impl Reaction {
    pub fn apply(
        conversation_id: &[u8; 16], dispatch_id: &[u8], reactor: &[u8; 32], emoji: &str, add: bool,
        timestamp: u64,
    ) -> bool {
        let conn = core().db.messages().lock();
        if crate::privacy::is_private_tx(&conn, conversation_id).unwrap_or(true) {
            return private_apply_tx(
                &conn,
                conversation_id,
                dispatch_id,
                reactor,
                emoji,
                add,
                timestamp,
            )
            .is_ok();
        }
        let n = if add {
            conn.execute(
                "INSERT OR REPLACE INTO reactions (conversation_id, dispatch_id, reactor, emoji, timestamp) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (conversation_id.as_slice(), dispatch_id, reactor.as_slice(), emoji, timestamp),
            )
        } else {
            conn.execute(
                "DELETE FROM reactions \
                 WHERE conversation_id = ?1 AND dispatch_id = ?2 AND reactor = ?3 AND emoji = ?4",
                (conversation_id.as_slice(), dispatch_id, reactor.as_slice(), emoji),
            )
        };
        n.unwrap_or(0) > 0
    }

    pub fn for_conversation(conversation_id: &[u8; 16]) -> Vec<ReactionRow> {
        let conn = core().db.messages().lock();
        if crate::privacy::is_private_tx(&conn, conversation_id).unwrap_or(true) {
            let operations: Vec<Vec<u8>> = all(
                &conn,
                "SELECT operation FROM private_reactions WHERE conversation_id=?1 ORDER BY id",
                [conversation_id],
                |r| r.get(0),
            )
            .unwrap_or_default();
            let mut rows: Vec<ReactionRow> = Vec::new();
            for op in operations {
                let Ok(bytes) = crate::privacy::open_bytes(conversation_id, &op) else {
                    return Vec::new();
                };
                let Ok((dispatch_id, reactor, emoji, add, timestamp)) =
                    postcard::from_bytes::<(Vec<u8>, [u8; 32], String, bool, u64)>(&bytes)
                else {
                    return Vec::new();
                };
                rows.retain(|r| {
                    !(r.dispatch_id == dispatch_id && r.reactor == reactor && r.emoji == emoji)
                });
                if add {
                    rows.push(ReactionRow {
                        conversation_id: *conversation_id,
                        dispatch_id,
                        reactor,
                        emoji,
                        timestamp,
                    });
                }
            }
            return rows;
        }
        drop(conn);
        all(
            &core().db.messages().lock(),
            "SELECT * FROM reactions WHERE conversation_id = ?1 ORDER BY timestamp ASC",
            [conversation_id.as_slice()],
            ReactionRow::from_row,
        )
        .unwrap_or_default()
    }

    pub fn dump_all_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<ReactionRow>> {
        all(
            conn,
            "SELECT * FROM reactions WHERE conversation_id NOT IN (SELECT conversation_id FROM private_conversations)",
            [],
            ReactionRow::from_row,
        )
    }

    pub fn import_rows_tx(
        conn: &rusqlite::Connection, rows: &[ReactionRow], replace: bool,
    ) -> anyhow::Result<usize> {
        let sql = format!(
            "INSERT OR {} INTO reactions (conversation_id, dispatch_id, reactor, emoji, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            if replace { "REPLACE" } else { "IGNORE" },
        );
        let mut n = 0usize;
        for r in rows {
            if crate::privacy::is_private_tx(conn, &r.conversation_id)? {
                continue;
            }
            n += conn.execute(
                &sql,
                (
                    r.conversation_id.as_slice(),
                    &r.dispatch_id,
                    r.reactor.as_slice(),
                    &r.emoji,
                    r.timestamp,
                ),
            )?;
        }
        Ok(n)
    }
}

pub(crate) fn private_apply_tx(
    conn: &rusqlite::Connection, conv: &[u8; 16], did: &[u8], reactor: &[u8; 32], emoji: &str,
    add: bool, timestamp: u64,
) -> anyhow::Result<()> {
    let bytes = postcard::to_allocvec(&(did, reactor, emoji, add, timestamp))?;
    conn.execute(
        "INSERT INTO private_reactions(conversation_id,operation) VALUES(?1,?2)",
        (conv.as_slice(), crate::privacy::seal_bytes_tx(conn, conv, &bytes)?),
    )?;
    Ok(())
}
