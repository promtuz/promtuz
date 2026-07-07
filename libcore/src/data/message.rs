use anyhow::Result;
use parking_lot::Mutex;
use ulid::Ulid;

use crate::db::messages::MESSAGES_DB;
use crate::db::messages::MessageRow;
use crate::utils::systime;

/// Message status constants
pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SENT: u8 = 1;
pub const STATUS_FAILED: u8 = 2;

/// Strictly-monotonic 16-byte dispatch id. `Uuid::now_v7()` is only
/// millisecond-monotonic (random tail), so two sends in the same ms don't
/// order by send time — which would let a "delivered up to X" watermark
/// mark a not-yet-delivered sibling. Clamp each mint to strictly greater
/// than the last. Serialized on one device by this lock (cheap).
// ponytail: process-local monotonic; a burst can push the id's ts bits a
// hair ahead of wall-clock — harmless, it's a sortable token, not a clock.
static LAST_DISPATCH_ID: Mutex<u128> = Mutex::new(0);

pub fn next_dispatch_id() -> [u8; 16] {
    let mut last = LAST_DISPATCH_ID.lock();
    let mut v = u128::from_be_bytes(uuid::Uuid::now_v7().into_bytes());
    if v <= *last {
        v = *last + 1;
    }
    *last = v;
    v.to_be_bytes()
}

#[derive(Debug, Clone)]
pub struct Message {
    pub inner: MessageRow,
}
/// FIXME:
/// This code is bullshit crap written by AI
impl Message {
    /// Save an outgoing message (status = pending until relay confirms).
    pub fn save_outgoing(peer_ipk: [u8; 32], content: &str) -> Result<Self> {
        let id = Ulid::new();
        let timestamp = systime().as_secs();
        let dispatch_id = next_dispatch_id();
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_PENDING, dispatch_id.as_slice()),
        )?;

        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                peer_ipk,
                content: content.to_string(),
                outgoing: true,
                timestamp,
                status: STATUS_PENDING,
                dispatch_id: dispatch_id.to_vec(),
            },
        })
    }

    /// Save an incoming (received) message. `dispatch_id` is the sender's
    /// monotonic id; `ON CONFLICT` makes redelivery a no-op — `Ok(None)`
    /// tells the caller "already have it", not an error.
    pub fn save_incoming(
        peer_ipk: [u8; 32], dispatch_id: &[u8; 16], content: &str, timestamp: u64,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let conn = MESSAGES_DB.lock();
        let changed = conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)
             ON CONFLICT(peer_ipk, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_SENT, dispatch_id.as_slice()),
        )?;

        if changed == 0 {
            return Ok(None);
        }

        Ok(Some(Self {
            inner: MessageRow {
                id: id.into(),
                peer_ipk,
                content: content.to_string(),
                outgoing: false,
                timestamp,
                status: STATUS_SENT,
                dispatch_id: dispatch_id.to_vec(),
            },
        }))
    }

    /// Mark an outgoing message as sent (relay accepted).
    pub fn mark_sent(id: &Ulid) {
        let conn = MESSAGES_DB.lock();
        conn.execute("UPDATE messages SET status = ?1 WHERE id = ?2", (STATUS_SENT, id.to_string()))
            .ok();
    }

    /// Mark an outgoing message as failed.
    pub fn mark_failed(id: &Ulid) {
        let conn = MESSAGES_DB.lock();
        conn.execute("UPDATE messages SET status = ?1 WHERE id = ?2", (STATUS_FAILED, id.to_string()))
            .ok();
    }

    /// Get messages for a conversation, paginated.
    /// Returns messages in ascending order (oldest first).
    /// `before_id` if non-empty, fetches messages before that ULID.
    pub fn get_messages(peer_ipk: &[u8; 32], limit: u32, before_id: &str) -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();

        if !before_id.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM messages WHERE peer_ipk = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
                )
                .expect("failed to prepare");
            let mut rows: Vec<MessageRow> = stmt
                .query_map((peer_ipk.as_slice(), before_id, limit), MessageRow::from_row)
                .expect("failed to query")
                .filter_map(|r| r.ok())
                .collect();
            rows.reverse();
            rows
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT * FROM messages WHERE peer_ipk = ?1 ORDER BY id DESC LIMIT ?2",
                )
                .expect("failed to prepare");
            let mut rows: Vec<MessageRow> = stmt
                .query_map((peer_ipk.as_slice(), limit), MessageRow::from_row)
                .expect("failed to query")
                .filter_map(|r| r.ok())
                .collect();
            rows.reverse();
            rows
        }
    }

    /// Get a summary of all conversations (one entry per peer, with the latest message).
    pub fn get_conversations() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.* FROM messages m
                 INNER JOIN (
                     SELECT peer_ipk, MAX(id) AS max_id FROM messages GROUP BY peer_ipk
                 ) latest ON m.id = latest.max_id
                 ORDER BY m.id DESC",
            )
            .expect("failed to prepare");
        stmt.query_map([], MessageRow::from_row)
            .expect("failed to query")
            .filter_map(|r| r.ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_id_is_monotonic() {
        let a = next_dispatch_id();
        let b = next_dispatch_id();
        assert!(b > a, "ids must strictly increase");
    }

    /// `save_incoming` runs through the process-global `MESSAGES_DB`
    /// Lazy, which is fragile to test directly (path resolves once from
    /// `PROMTUZ_DATA_DIR`). Exercise the same SQL against an in-memory
    /// connection instead: the `(peer_ipk, dispatch_id)` partial unique
    /// index + `ON CONFLICT DO NOTHING` is exactly what `save_incoming`
    /// relies on for idempotence.
    #[test]
    fn save_incoming_dedups_on_dispatch_id() {
        let conn = crate::db::messages::open_in_memory();
        let peer = [7u8; 32];
        let did = [1u8; 16];
        let sql = "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id) \
                   VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6) \
                   ON CONFLICT(peer_ipk, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING";

        let first = conn
            .execute(
                sql,
                (Ulid::new().to_string(), peer.as_slice(), "hi", 100u64, STATUS_SENT, did.as_slice()),
            )
            .unwrap();
        let dup = conn
            .execute(
                sql,
                (Ulid::new().to_string(), peer.as_slice(), "hi", 100u64, STATUS_SENT, did.as_slice()),
            )
            .unwrap();

        assert_eq!(first, 1, "first insert must land");
        assert_eq!(dup, 0, "same (peer, dispatch_id) must not double-insert");
    }
}
