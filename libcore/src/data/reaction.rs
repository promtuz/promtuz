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
        all(
            &core().db.messages().lock(),
            "SELECT * FROM reactions WHERE conversation_id = ?1 ORDER BY timestamp ASC",
            [conversation_id.as_slice()],
            ReactionRow::from_row,
        )
        .unwrap_or_default()
    }

    pub fn dump_all_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<ReactionRow>> {
        all(conn, "SELECT * FROM reactions", [], ReactionRow::from_row)
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
            n += conn.execute(
                &sql,
                (r.conversation_id.as_slice(), &r.dispatch_id, r.reactor.as_slice(), &r.emoji, r.timestamp),
            )?;
        }
        Ok(n)
    }
}
