//! Emoji reactions — a side-table next to `messages` (same DB/connection).
//!
//! Keyed on the reactor's IPK (not a me/them bool) so a multi-member group
//! attributes each reaction to its author. `add = false` removes exactly one
//! `(reactor, emoji)` pair; `add = true` upserts it. Multiple distinct emoji
//! per reactor per message are allowed; the same emoji twice is idempotent.

use crate::db::messages::MESSAGES_DB;
use crate::db::messages::ReactionRow;

pub struct Reaction;

impl Reaction {
    /// Apply one reaction change. Returns `true` if the table actually changed
    /// (so callers only emit an event / redraw on a real delta).
    pub fn apply(
        peer_ipk: &[u8; 32], dispatch_id: &[u8], reactor: &[u8; 32], emoji: &str, add: bool,
        timestamp: u64,
    ) -> bool {
        let conn = MESSAGES_DB.lock();
        let n = if add {
            conn.execute(
                "INSERT OR REPLACE INTO reactions (peer_ipk, dispatch_id, reactor, emoji, timestamp) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (peer_ipk.as_slice(), dispatch_id, reactor.as_slice(), emoji, timestamp),
            )
        } else {
            conn.execute(
                "DELETE FROM reactions \
                 WHERE peer_ipk = ?1 AND dispatch_id = ?2 AND reactor = ?3 AND emoji = ?4",
                (peer_ipk.as_slice(), dispatch_id, reactor.as_slice(), emoji),
            )
        };
        n.unwrap_or(0) > 0
    }

    /// All reactions in a conversation, oldest first. The UI groups by
    /// `dispatch_id` and marks `reactor == self` as its own.
    pub fn for_peer(peer_ipk: &[u8; 32]) -> Vec<ReactionRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT peer_ipk, dispatch_id, reactor, emoji, timestamp FROM reactions \
             WHERE peer_ipk = ?1 ORDER BY timestamp ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([peer_ipk.as_slice()], ReactionRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Drop every reaction in a conversation (forget-contact cascade).
    pub fn delete_by_peer(peer_ipk: &[u8; 32]) {
        let conn = MESSAGES_DB.lock();
        conn.execute("DELETE FROM reactions WHERE peer_ipk = ?1", [peer_ipk.as_slice()]).ok();
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    /// `Reaction::apply` runs on the process-global `MESSAGES_DB`; exercise the
    /// same PK semantics against an in-memory connection. This is the
    /// group-correctness guarantee: distinct (reactor, emoji) pairs coexist,
    /// the same pair is idempotent, and a remove deletes exactly one pair.
    fn add(conn: &Connection, msg: &[u8], reactor: &[u8; 32], emoji: &str) -> usize {
        conn.execute(
            "INSERT OR REPLACE INTO reactions (peer_ipk, dispatch_id, reactor, emoji, timestamp) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (&[1u8; 32][..], msg, reactor.as_slice(), emoji, 0u64),
        )
        .unwrap()
    }
    fn count(conn: &Connection, msg: &[u8]) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM reactions WHERE dispatch_id = ?1", [msg], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn reaction_pk_is_group_correct() {
        let conn = crate::db::messages::open_in_memory();
        let msg = [9u8; 16];
        let a = [0xAAu8; 32];
        let b = [0xBBu8; 32];

        add(&conn, &msg, &a, "👍");
        add(&conn, &msg, &b, "👍"); // different reactor, same emoji → coexists
        add(&conn, &msg, &a, "🔥"); // same reactor, different emoji → coexists
        assert_eq!(count(&conn, &msg), 3);

        add(&conn, &msg, &a, "👍"); // same (reactor, emoji) → idempotent upsert
        assert_eq!(count(&conn, &msg), 3);

        conn.execute(
            "DELETE FROM reactions WHERE peer_ipk = ?1 AND dispatch_id = ?2 AND reactor = ?3 AND emoji = ?4",
            (&[1u8; 32][..], &msg[..], a.as_slice(), "👍"),
        )
        .unwrap();
        assert_eq!(count(&conn, &msg), 2, "remove drops exactly the one pair");
    }
}
