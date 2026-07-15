use anyhow::Result;
use parking_lot::Mutex;
use ulid::Ulid;

use crate::db::messages::MESSAGES_DB;
use crate::db::messages::MessageRow;
use crate::utils::systime;

/// Message status constants. Higher = further along; receipts only ever
/// upgrade (never downgrade) an outgoing message's status.
pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SENT: u8 = 1;
pub const STATUS_FAILED: u8 = 2;
pub const STATUS_DELIVERED: u8 = 3;
pub const STATUS_READ: u8 = 4;

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
    /// `reply_to` is the quoted message's dispatch_id, if this is a reply.
    pub fn save_outgoing(
        peer_ipk: [u8; 32], content: &str, reply_to: Option<[u8; 16]>,
    ) -> Result<Self> {
        let id = Ulid::new();
        let timestamp = systime().as_secs();
        let dispatch_id = next_dispatch_id();
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id, reply_to) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_PENDING, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
        )?;

        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                peer_ipk,
                content: content.to_string(),
                outgoing: true,
                timestamp,
                status: STATUS_PENDING,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
            },
        })
    }

    /// Save an incoming (received) message. `dispatch_id` is the sender's
    /// monotonic id; `ON CONFLICT` makes redelivery a no-op — `Ok(None)`
    /// tells the caller "already have it", not an error.
    pub fn save_incoming(
        peer_ipk: [u8; 32], dispatch_id: &[u8; 16], content: &str, timestamp: u64,
        reply_to: Option<[u8; 16]>,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let conn = MESSAGES_DB.lock();
        let changed = conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id, reply_to) VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7)
             ON CONFLICT(peer_ipk, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_SENT, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
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
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
            },
        }))
    }

    /// Mark an outgoing message as sent (relay accepted).
    pub fn mark_sent(id: &Ulid, timestamp: u64) {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "UPDATE messages SET status = ?1, timestamp = ?2 WHERE id = ?3",
            (STATUS_SENT, timestamp, id.to_string()),
        )
            .ok();
    }

    /// Mark an outgoing message as failed.
    pub fn mark_failed(id: &Ulid) {
        let conn = MESSAGES_DB.lock();
        conn.execute("UPDATE messages SET status = ?1 WHERE id = ?2", (STATUS_FAILED, id.to_string()))
            .ok();
    }

    /// Set an outgoing message's status by its `dispatch_id`, returning the
    /// updated row. The async reconciler holds the `dispatch_id` (the outbox
    /// key), not the local ULID, so this is how it reflects a
    /// delivered/failed outcome back onto the message the UI reads.
    pub fn mark_by_dispatch_id(
        dispatch_id: &[u8], status: u8, timestamp: Option<u64>,
    ) -> Option<MessageRow> {
        let conn = MESSAGES_DB.lock();
        // Scope to outgoing rows: dispatch_id is globally monotonic among OUR
        // sends (unique), but an incoming message carries a *peer's* dispatch_id
        // and could in principle collide — never touch those.
        conn.execute(
            "UPDATE messages SET status = ?1, timestamp = COALESCE(?2, timestamp) WHERE dispatch_id = ?3 AND outgoing = 1",
            (status, timestamp, dispatch_id),
        )
        .ok()?;
        conn.query_row(
            "SELECT * FROM messages WHERE dispatch_id = ?1 AND outgoing = 1",
            [dispatch_id],
            MessageRow::from_row,
        )
        .ok()
    }

    /// Apply an edit — our own (optimistic) or an inbound peer `Edit`: replace
    /// the target's text and flag it edited. `own` is the authorship guard:
    /// only the author may edit a message, so a local edit passes `true`
    /// (touches our `outgoing = 1` rows) and an inbound peer edit passes
    /// `false` (touches only rows the peer sent us, `outgoing = 0`). Without
    /// it a peer could rewrite a message WE authored — it knows our
    /// dispatch_ids from the wire. No-op on an already-deleted target. Returns
    /// the updated row (for the UI event), or `None` if unauthorized/absent.
    pub fn apply_edit(
        peer_ipk: &[u8; 32], dispatch_id: &[u8], content: &str, own: bool,
    ) -> Option<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let n = conn
            .execute(
                "UPDATE messages SET content = ?1, edited = 1 \
                 WHERE peer_ipk = ?2 AND dispatch_id = ?3 AND outgoing = ?4 AND deleted = 0",
                (content, peer_ipk.as_slice(), dispatch_id, own),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        conn.query_row(
            "SELECT * FROM messages WHERE peer_ipk = ?1 AND dispatch_id = ?2",
            (peer_ipk.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Tombstone a message (delete-for-everyone): clear its text, flag deleted.
    /// Same authorship guard as [`Self::apply_edit`] — `own = true` for our own
    /// delete, `false` for an inbound peer delete — so neither side can
    /// tombstone the other's authored messages. Returns the updated row.
    pub fn apply_delete(peer_ipk: &[u8; 32], dispatch_id: &[u8], own: bool) -> Option<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let n = conn
            .execute(
                "UPDATE messages SET content = '', deleted = 1, edited = 0 \
                 WHERE peer_ipk = ?1 AND dispatch_id = ?2 AND outgoing = ?3",
                (peer_ipk.as_slice(), dispatch_id, own),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        conn.query_row(
            "SELECT * FROM messages WHERE peer_ipk = ?1 AND dispatch_id = ?2",
            (peer_ipk.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Hard-delete a single message locally (delete-for-me; no wire signal).
    /// Returns the row it removed (for the UI event), or `None` if absent.
    pub fn hard_delete(peer_ipk: &[u8; 32], dispatch_id: &[u8]) -> Option<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let row = conn
            .query_row(
                "SELECT * FROM messages WHERE peer_ipk = ?1 AND dispatch_id = ?2",
                (peer_ipk.as_slice(), dispatch_id),
                MessageRow::from_row,
            )
            .ok()?;
        conn.execute(
            "DELETE FROM messages WHERE peer_ipk = ?1 AND dispatch_id = ?2",
            (peer_ipk.as_slice(), dispatch_id),
        )
        .ok()?;
        Some(row)
    }

    /// Apply a receipt high-water-mark: upgrade every outgoing message to
    /// `peer` with `dispatch_id <= upto` to at-least `status` (never
    /// downgrades). One receipt clears a whole backlog. `dispatch_id` is
    /// 16-byte big-endian, so the BLOB `<=` compare matches send order.
    /// Returns `true` if any row changed. Group note: 1:1 today — a group
    /// would key this per member and aggregate.
    pub fn mark_receipt_upto(peer_ipk: &[u8; 32], upto: &[u8; 16], status: u8) -> bool {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "UPDATE messages SET status = ?1 \
             WHERE peer_ipk = ?2 AND outgoing = 1 AND status < ?1 \
             AND dispatch_id IS NOT NULL AND dispatch_id <= ?3",
            (status, peer_ipk.as_slice(), upto.as_slice()),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    /// Every message, oldest first — the backup dump (IDENTITY_RECOVERY.md §4).
    pub fn dump_all() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let Ok(mut stmt) = conn.prepare("SELECT * FROM messages ORDER BY id ASC") else {
            return Vec::new();
        };
        stmt.query_map([], MessageRow::from_row)
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    /// Restore dumped rows in one transaction. `INSERT OR IGNORE` — the ULID
    /// PK plus the `(peer_ipk, dispatch_id)` partial index make re-imports
    /// idempotent. Returns rows actually inserted.
    pub fn import_rows(rows: &[MessageRow]) -> Result<usize> {
        let mut conn = MESSAGES_DB.lock();
        let tx = conn.transaction()?;
        let mut n = 0usize;
        for r in rows {
            n += tx.execute(
                "INSERT OR IGNORE INTO messages \
                 (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                (
                    &r.id,
                    r.peer_ipk.as_slice(),
                    &r.content,
                    r.outgoing,
                    r.timestamp,
                    r.status,
                    &r.dispatch_id,
                    r.edited,
                    r.deleted,
                ),
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Delete every message with this peer (forget-contact cascade).
    pub fn delete_by_peer(peer_ipk: &[u8; 32]) {
        let conn = MESSAGES_DB.lock();
        conn.execute("DELETE FROM messages WHERE peer_ipk = ?1", [peer_ipk.as_slice()]).ok();
    }

    /// Fail every not-yet-read outgoing message to this peer (PAIRING.md): a
    /// declined pair means our PENDING-era sends were encrypted to a group the
    /// peer never joined, so they can never arrive. Skips already-read/delivered
    /// (status > sent) defensively. Rides the reactive doorbell.
    pub fn mark_all_failed_by_peer(peer_ipk: &[u8; 32]) {
        let conn = MESSAGES_DB.lock();
        let _ = conn.execute(
            "UPDATE messages SET status = ?1 WHERE peer_ipk = ?2 AND outgoing = 1 AND status <= ?3",
            (STATUS_FAILED, peer_ipk.as_slice(), STATUS_SENT),
        );
    }

    /// Count of messages with this peer (cheap diagnostics read).
    pub fn count_by_peer(peer_ipk: &[u8; 32]) -> u32 {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE peer_ipk = ?1",
            [peer_ipk.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u32)
        .unwrap_or(0)
    }

    /// Status of the newest message with this peer, or `None` if none.
    pub fn last_status_by_peer(peer_ipk: &[u8; 32]) -> Option<u8> {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT status FROM messages WHERE peer_ipk = ?1 ORDER BY id DESC LIMIT 1",
            [peer_ipk.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .ok()
        .map(|s| s as u8)
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

    /// Outgoing rows still pending (status = 0) — the durable-first-send
    /// retry set. Oldest-first by ULID so a reconnect re-sends in send order.
    pub fn pending_outgoing() -> Vec<MessageRow> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare("SELECT * FROM messages WHERE outgoing = 1 AND status = 0 ORDER BY id ASC")
            .expect("failed to prepare");
        stmt.query_map([], MessageRow::from_row)
            .expect("failed to query")
            .filter_map(|r| r.ok())
            .collect()
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

    /// Advance the local read high-water-mark for `peer` to `upto` (a 16-byte
    /// dispatch id). Monotonic — a BLOB compare keeps it from moving backwards
    /// (dispatch ids are big-endian, so memcmp == send order). Writes
    /// MESSAGES_DB, so it rings the reactive doorbell and the home unread
    /// count re-reads.
    pub fn set_read_watermark(peer_ipk: &[u8; 32], upto: &[u8; 16]) {
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "INSERT INTO read_state (peer_ipk, upto_dispatch_id) VALUES (?1, ?2)
             ON CONFLICT(peer_ipk) DO UPDATE SET upto_dispatch_id = excluded.upto_dispatch_id
             WHERE excluded.upto_dispatch_id > read_state.upto_dispatch_id",
            (peer_ipk.as_slice(), upto.as_slice()),
        )
        .ok();
    }

    /// Newest incoming (dispatch-bearing) message's id for `peer` — the
    /// watermark target when marking a whole conversation read.
    pub fn newest_incoming_dispatch(peer_ipk: &[u8; 32]) -> Option<[u8; 16]> {
        let conn = MESSAGES_DB.lock();
        conn.query_row(
            "SELECT dispatch_id FROM messages
             WHERE peer_ipk = ?1 AND outgoing = 0 AND dispatch_id IS NOT NULL
             ORDER BY dispatch_id DESC LIMIT 1",
            [peer_ipk.as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .ok()
        .and_then(|v| v.try_into().ok())
    }

    /// Unread incoming count per peer: incoming, non-deleted, dispatch-bearing
    /// messages newer than the peer's read watermark. Only peers with unread > 0.
    pub fn unread_counts() -> Vec<([u8; 32], u32)> {
        let conn = MESSAGES_DB.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.peer_ipk, COUNT(*) FROM messages m
                 LEFT JOIN read_state r ON r.peer_ipk = m.peer_ipk
                 WHERE m.outgoing = 0 AND m.deleted = 0 AND m.dispatch_id IS NOT NULL
                   AND (r.upto_dispatch_id IS NULL OR m.dispatch_id > r.upto_dispatch_id)
                 GROUP BY m.peer_ipk",
            )
            .expect("failed to prepare");
        stmt.query_map([], |row| {
            let peer: Vec<u8> = row.get(0)?;
            let count: u32 = row.get(1)?;
            Ok((peer.try_into().unwrap_or([0u8; 32]), count))
        })
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

    /// The receipt high-water-mark: `dispatch_id <= upto` must order by the
    /// 16-byte BE id (so one receipt covers the backlog), and `status < ?` must
    /// never downgrade (a later Delivered can't undo a Read). Mirrors
    /// `mark_receipt_upto`'s SQL against an in-memory DB (the method uses the
    /// process-global connection).
    #[test]
    fn receipt_watermark_covers_backlog_without_downgrade() {
        let conn = crate::db::messages::open_in_memory();
        let peer = [7u8; 32];
        let ids: [[u8; 16]; 3] = [[1u8; 16], [2u8; 16], [3u8; 16]];
        for (i, did) in ids.iter().enumerate() {
            conn.execute(
                "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status, dispatch_id) \
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)",
                (Ulid::new().to_string(), peer.as_slice(), "m", i as u64, STATUS_SENT, did.as_slice()),
            )
            .unwrap();
        }
        let mark = |upto: &[u8; 16], status: u8| {
            conn.execute(
                "UPDATE messages SET status = ?1 \
                 WHERE peer_ipk = ?2 AND outgoing = 1 AND status < ?1 \
                 AND dispatch_id IS NOT NULL AND dispatch_id <= ?3",
                (status, peer.as_slice(), upto.as_slice()),
            )
            .unwrap()
        };
        let status_of = |did: &[u8; 16]| -> u8 {
            conn.query_row(
                "SELECT status FROM messages WHERE dispatch_id = ?1",
                [did.as_slice()],
                |r| r.get::<_, i64>(0),
            )
            .map(|s| s as u8)
            .unwrap()
        };

        assert_eq!(mark(&ids[1], STATUS_DELIVERED), 2, "covers ids[0] and ids[1]");
        assert_eq!(status_of(&ids[0]), STATUS_DELIVERED);
        assert_eq!(status_of(&ids[1]), STATUS_DELIVERED);
        assert_eq!(status_of(&ids[2]), STATUS_SENT, "beyond watermark, untouched");

        mark(&ids[2], STATUS_READ); // read the lot
        assert_eq!(status_of(&ids[2]), STATUS_READ);
        mark(&ids[2], STATUS_DELIVERED); // stale Delivered must not downgrade
        assert_eq!(status_of(&ids[0]), STATUS_READ, "no downgrade below current");
    }

    /// A row written before the `dispatch_id` column existed has NULL there.
    /// `MessageRow::from_row` must decode NULL → `None`, not error — otherwise
    /// the `filter_map(Result::ok)` readers silently drop every legacy row.
    #[test]
    fn legacy_null_dispatch_id_row_reads_back() {
        let conn = crate::db::messages::open_in_memory();
        conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status) \
             VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            (Ulid::new().to_string(), [9u8; 32].as_slice(), "legacy", 42u64, STATUS_SENT),
        )
        .unwrap();

        let row = conn.query_row("SELECT * FROM messages", [], MessageRow::from_row).unwrap();
        assert_eq!(row.dispatch_id, None, "NULL dispatch_id must decode to None");
    }
}
