use anyhow::Result;
use common::utils::now_secs;
use parking_lot::Mutex;
use ulid::Ulid;

use crate::data::backup::MemberReadRow;
use crate::data::backup::ReadRow;
use crate::db::all;
use crate::db::messages::MessageRow;
use crate::db::one;
use crate::state::core;

/// Failed is a send outcome, not progress past sent; delivery and read evidence outrank it.
pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SENT: u8 = 1;
pub const STATUS_FAILED: u8 = 2;
pub const STATUS_DELIVERED: u8 = 3;
pub const STATUS_READ: u8 = 4;

/// Strictly monotonic, so a "delivered up to X" watermark never covers a later same-ms send.
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

impl MessageRow {
    /// Outgoing rows store no sender, so they resolve to `me`.
    pub fn sender(&self, me: &[u8; 32]) -> [u8; 32] {
        self.sender_ipk
            .as_ref()
            .and_then(|v| v.as_slice().try_into().ok())
            .unwrap_or(*me)
    }
}

impl Message {
    pub fn save_outgoing(
        conversation_id: [u8; 16], content: &str, reply_to: Option<[u8; 16]>,
    ) -> Result<Self> {
        let me = super::identity::Identity::local_ipk();
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        let row = Self::save_outgoing_tx(&tx, conversation_id, content, reply_to, me)?;
        tx.commit()?;
        Ok(row)
    }

    pub fn save_outgoing_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], content: &str,
        reply_to: Option<[u8; 16]>, me: Option<[u8; 32]>,
    ) -> Result<Self> {
        let id = Ulid::new();
        let timestamp = now_secs();
        let dispatch_id = next_dispatch_id();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, content, outgoing, timestamp, status, dispatch_id, reply_to) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
            (&id.to_string(), conversation_id.as_slice(), content, timestamp, STATUS_PENDING, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
        )?;

        // Snapshot failure must roll the message back, not freeze an empty
        // audience. Resolve identity before locking this DB at the call site.
        let recipients =
            super::conversation::Conversation::recipients_tx(conn, &conversation_id, me)?;
        super::receipts::snapshot_tx(conn, &id.to_string(), &recipients, true)?;
        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: None,
                content: content.to_string(),
                outgoing: true,
                timestamp,
                status: STATUS_PENDING,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
                system: crate::db::messages::SYSTEM_NONE,
            },
        })
    }

    /// `sender` is the MLS leaf credential that wrote it, not the outer envelope sender.
    /// `Ok(None)` means it is already stored or was deleted.
    pub fn save_incoming(
        conversation_id: [u8; 16], sender: [u8; 32], dispatch_id: &[u8; 16], content: &str,
        timestamp: u64, reply_to: Option<[u8; 16]>,
    ) -> Result<Option<Self>> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        let row = Self::save_incoming_tx(
            &tx,
            conversation_id,
            sender,
            dispatch_id,
            content,
            timestamp,
            reply_to,
        )?;
        tx.commit()?;
        Ok(row)
    }

    pub fn save_incoming_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], sender: [u8; 32],
        dispatch_id: &[u8; 16], content: &str, timestamp: u64, reply_to: Option<[u8; 16]>,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let changed = conn.execute(
            "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, reply_to, notification_seen)
             SELECT ?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?8, 0
             WHERE NOT EXISTS (SELECT 1 FROM message_deletions
                 WHERE conversation_id = ?2 AND sender_ipk = ?3 AND dispatch_id = ?7)
             ON CONFLICT(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (&id.to_string(), conversation_id.as_slice(), sender.as_slice(), content, timestamp, STATUS_SENT, dispatch_id.as_slice(), reply_to.as_ref().map(|r| r.as_slice())),
        )?;

        if changed == 0 {
            return Ok(None);
        }

        super::receipts::arrived_tx(conn, &id.to_string(), now_secs())?;
        Ok(Some(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: Some(sender.to_vec()),
                content: content.to_string(),
                outgoing: false,
                timestamp,
                status: STATUS_SENT,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: reply_to.map(|r| r.to_vec()),
                system: crate::db::messages::SYSTEM_NONE,
            },
        }))
    }

    pub fn get_by_dispatch(conversation_id: &[u8; 16], dispatch_id: &[u8; 16]) -> Option<Self> {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = 1",
            (conversation_id.as_slice(), dispatch_id.as_slice()),
            MessageRow::from_row,
        )
        .ok()
        .map(|inner| Self { inner })
    }

    /// Fail unaccepted recipients without overwriting anyone's receipt.
    pub fn mark_failed(id: &Ulid) {
        if let Err(e) = super::receipts::fail_pending(&id.to_string()) {
            log::warn!("MESSAGE: could not persist failure: {e}");
        }
    }

    /// `own` and `author` guard authorship, since peers know our dispatch ids from the wire.
    pub fn apply_edit(
        conversation_id: &[u8; 16], dispatch_id: &[u8], content: &str, own: bool,
        author: Option<&[u8; 32]>,
    ) -> Option<MessageRow> {
        let conn = core().db.messages().lock();
        let n = conn
            .execute(
                "UPDATE messages SET content = ?1, edited = 1 \
                 WHERE conversation_id = ?2 AND dispatch_id = ?3 AND outgoing = ?4 AND deleted = 0 \
                   AND (?5 IS NULL OR sender_ipk = ?5)",
                (
                    content,
                    conversation_id.as_slice(),
                    dispatch_id,
                    own,
                    author.map(|a| a.as_slice()),
                ),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Delete for everyone, media included, under the same guard as [`Self::apply_edit`].
    pub fn apply_delete(
        conversation_id: &[u8; 16], dispatch_id: &[u8], own: bool, author: Option<&[u8; 32]>,
    ) -> Option<MessageRow> {
        if !own && let Some(author) = author {
            return Self::receive_delete(conversation_id, &dispatch_id.try_into().ok()?, author)
                .ok()
                .flatten();
        }
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction().ok()?;
        let n = tx
            .execute(
                "UPDATE messages SET content = '', deleted = 1, edited = 0 \
                 WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = ?3 \
                   AND (?4 IS NULL OR sender_ipk = ?4)",
                (conversation_id.as_slice(), dispatch_id, own, author.map(|a| a.as_slice())),
            )
            .ok()?;
        if n == 0 {
            return None;
        }
        let orphan = crate::data::media::drop_row_tx(&tx, conversation_id, dispatch_id).ok()?;
        tx.commit().ok()?;
        crate::data::media::unlink_orphaned(&core().db, &conn, orphan.as_slice());
        conn.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
            MessageRow::from_row,
        )
        .ok()
    }

    /// Records the author's deletion even before the post arrives, so it never appears. Files are
    /// unlinked only after commit.
    pub(crate) fn receive_delete(
        conversation_id: &[u8; 16], dispatch_id: &[u8; 16], author: &[u8; 32],
    ) -> Result<Option<MessageRow>> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        let (row, orphan) = Self::receive_delete_tx(&tx, conversation_id, dispatch_id, author)?;
        tx.commit()?;
        crate::data::media::unlink_orphaned(&core().db, &conn, orphan.as_slice());
        Ok(row)
    }

    fn receive_delete_tx(
        tx: &rusqlite::Transaction<'_>, conversation_id: &[u8; 16], dispatch_id: &[u8; 16],
        author: &[u8; 32],
    ) -> Result<(Option<MessageRow>, Option<[u8; 32]>)> {
        tx.execute(
            "INSERT OR IGNORE INTO message_deletions (conversation_id, sender_ipk, dispatch_id)
             VALUES (?1, ?2, ?3)",
            (conversation_id.as_slice(), author.as_slice(), dispatch_id.as_slice()),
        )?;
        let n = tx.execute(
            "UPDATE messages SET content = '', deleted = 1, edited = 0
             WHERE conversation_id = ?1 AND dispatch_id = ?2 AND outgoing = 0
               AND sender_ipk = ?3 AND deleted = 0",
            (conversation_id.as_slice(), dispatch_id.as_slice(), author.as_slice()),
        )?;
        if n == 0 {
            return Ok((None, None));
        }
        let orphan = crate::data::media::drop_row_tx(tx, conversation_id, dispatch_id)?;
        let row = tx.query_row(
            "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id.as_slice()),
            MessageRow::from_row,
        )?;
        Ok((Some(row), orphan))
    }

    /// Delete for me: local only, media included.
    pub fn hard_delete(conversation_id: &[u8; 16], dispatch_id: &[u8]) -> Option<MessageRow> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction().ok()?;
        let row = tx
            .query_row(
                "SELECT * FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
                (conversation_id.as_slice(), dispatch_id),
                MessageRow::from_row,
            )
            .ok()?;
        tx.execute(
            "DELETE FROM messages WHERE conversation_id = ?1 AND dispatch_id = ?2",
            (conversation_id.as_slice(), dispatch_id),
        )
        .ok()?;
        let orphan = crate::data::media::drop_row_tx(&tx, conversation_id, dispatch_id).ok()?;
        tx.commit().ok()?;
        crate::data::media::unlink_orphaned(&core().db, &conn, orphan.as_slice());
        Some(row)
    }

    /// Record every notice for one signed transition atomically. The marker
    /// survives clearing history, so recovery cannot recreate cleared notices.
    pub(crate) fn record_group_change(
        conversation: [u8; 16], change: [u8; 32], actor: [u8; 32], rows: &[(u8, String)], ts: u64,
    ) -> Result<Option<Vec<Self>>> {
        use sha2::Digest;
        use sha2::Sha256;
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        if tx.execute(
            "INSERT OR IGNORE INTO group_events(conversation_id,change_id) VALUES(?1,?2)",
            rusqlite::params![conversation, change],
        )? == 0
        {
            return Ok(None);
        }
        let ours = super::identity::Identity::get().is_some_and(|i| i.ipk() == actor);
        let mut saved = Vec::new();
        for (code, target) in rows {
            let hash =
                Sha256::digest(postcard::to_allocvec(&(conversation, change, code, target))?);
            let did: [u8; 16] = hash[..16].try_into()?;
            if let Some(row) = Self::save_system_tx(
                &tx,
                conversation,
                actor,
                &did,
                *code,
                target,
                ts,
                ours,
                Some(&change),
            )? {
                saved.push(row);
            }
        }
        tx.commit()?;
        Ok(Some(saved))
    }

    pub(crate) fn reconcile_group_events(
        conversation: &[u8; 16], accepted: &[[u8; 32]],
    ) -> Result<()> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        let stored: Vec<[u8; 32]> = all(
            &tx,
            "SELECT DISTINCT group_change FROM messages WHERE conversation_id=?1 AND group_change IS NOT NULL",
            [conversation],
            |r| r.get(0),
        )?;
        for change in stored {
            if !accepted.contains(&change) {
                tx.execute(
                    "DELETE FROM messages WHERE conversation_id=?1 AND group_change=?2",
                    rusqlite::params![conversation, change],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// `target`, stored as the content, is the affected member's hex IPK or the new title.
    pub fn save_system(
        conversation_id: [u8; 16], actor: [u8; 32], dispatch_id: &[u8; 16], system: u8,
        target: &str, timestamp: u64, outgoing: bool,
    ) -> Result<Option<Self>> {
        let conn = core().db.messages().lock();
        Self::save_system_tx(
            &conn,
            conversation_id,
            actor,
            dispatch_id,
            system,
            target,
            timestamp,
            outgoing,
            None,
        )
    }

    fn save_system_tx(
        conn: &rusqlite::Connection, conversation_id: [u8; 16], actor: [u8; 32],
        dispatch_id: &[u8; 16], system: u8, target: &str, timestamp: u64, outgoing: bool,
        group_change: Option<&[u8; 32]>,
    ) -> Result<Option<Self>> {
        let id = Ulid::new();
        let changed = conn.execute(
            "INSERT INTO messages (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, system, group_change) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(conversation_id, dispatch_id) WHERE dispatch_id IS NOT NULL DO NOTHING",
            (
                &id.to_string(),
                conversation_id.as_slice(),
                actor.as_slice(),
                target,
                outgoing,
                timestamp,
                STATUS_SENT,
                dispatch_id.as_slice(),
                system,
                group_change.map(|c| c.as_slice()),
            ),
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            inner: MessageRow {
                id: id.into(),
                conversation_id,
                sender_ipk: Some(actor.to_vec()),
                content: target.to_string(),
                outgoing,
                timestamp,
                status: STATUS_SENT,
                dispatch_id: Some(dispatch_id.to_vec()),
                edited: false,
                deleted: false,
                reply_to: None,
                system,
            },
        }))
    }

    pub fn dump_all_tx(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<MessageRow>> {
        all(conn, "SELECT * FROM messages ORDER BY id ASC", [], MessageRow::from_row)
    }

    pub fn import_rows_tx(conn: &rusqlite::Connection, rows: &[MessageRow]) -> Result<usize> {
        let mut n = 0usize;
        for r in rows {
            n += conn.execute(
                "INSERT OR IGNORE INTO messages \
                 (id, conversation_id, sender_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to, system) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                (
                    &r.id,
                    r.conversation_id.as_slice(),
                    &r.sender_ipk,
                    &r.content,
                    r.outgoing,
                    r.timestamp,
                    r.status,
                    &r.dispatch_id,
                    r.edited,
                    r.deleted,
                    &r.reply_to,
                    r.system,
                ),
            )?;
        }
        Ok(n)
    }

    /// After a declined pair: our sends went to a group the peer never joined.
    pub fn mark_all_failed_in(conversation_id: &[u8; 16]) {
        if let Err(e) = super::receipts::reject_conversation(conversation_id) {
            log::warn!("MESSAGE: could not persist declined-message outcomes: {e}");
        }
    }

    pub fn count_in(conversation_id: &[u8; 16]) -> u32 {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND deleted = 0",
            [conversation_id.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n as u32)
        .unwrap_or(0)
    }

    pub fn last_status_in(conversation_id: &[u8; 16]) -> Option<u8> {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT status FROM messages WHERE conversation_id = ?1 AND deleted = 0 ORDER BY id DESC LIMIT 1",
            [conversation_id.as_slice()],
            |r| r.get::<_, i64>(0),
        )
        .ok()
        .map(|s| s as u8)
    }

    pub fn get_messages(
        conversation_id: &[u8; 16], limit: u32, before_id: &str,
    ) -> Vec<MessageRow> {
        Self::get_messages_tx(&core().db.messages().lock(), conversation_id, limit, before_id)
            .unwrap_or_default()
    }

    pub(crate) fn get_messages_tx(
        conn: &rusqlite::Connection, conversation_id: &[u8; 16], limit: u32, before_id: &str,
    ) -> rusqlite::Result<Vec<MessageRow>> {
        let mut rows = if before_id.is_empty() {
            all(
                conn,
                "SELECT * FROM messages WHERE conversation_id = ?1 AND deleted = 0 ORDER BY id DESC LIMIT ?2",
                (conversation_id.as_slice(), limit),
                MessageRow::from_row,
            )
        } else {
            all(
                conn,
                "SELECT * FROM messages WHERE conversation_id = ?1 AND deleted = 0 AND id < ?2 ORDER BY id DESC LIMIT ?3",
                (conversation_id.as_slice(), before_id, limit),
                MessageRow::from_row,
            )
        }?;
        rows.reverse();
        Ok(rows)
    }

    /// Each hit carries how many messages are newer, so a window can widen to it. SQLite's LIKE
    /// folds case for ASCII only.
    pub fn search(conversation_id: &[u8; 16], query: &str, limit: u32) -> Vec<([u8; 16], u32)> {
        let needle = query.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let pattern =
            format!("%{}%", needle.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let hits = all(
            &core().db.messages().lock(),
            "SELECT m.dispatch_id, \
                    (SELECT COUNT(*) FROM messages n \
                      WHERE n.conversation_id = m.conversation_id AND n.deleted = 0 AND n.id > m.id) \
             FROM messages m \
             WHERE m.conversation_id = ?1 AND m.deleted = 0 AND m.system = 0 \
               AND m.dispatch_id IS NOT NULL AND m.content LIKE ?2 ESCAPE '\\' \
             ORDER BY m.id DESC LIMIT ?3",
            (conversation_id.as_slice(), pattern, limit),
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, u32>(1)?)),
        )
        .unwrap_or_default();
        // A restored blob can carry a dispatch id of any length; such a row has no hit to point at.
        hits.into_iter().filter_map(|(did, newer)| Some((did.try_into().ok()?, newer))).collect()
    }

    /// Window depth of the first message at or after `timestamp`, else of the latest.
    /// Timestamp and id order differ after an offline drain, so pick by timestamp, count by id.
    pub fn position_at_time(
        conversation_id: &[u8; 16], timestamp: u64,
    ) -> Result<Option<(String, Option<Vec<u8>>, u32)>> {
        position_at_time(&core().db.messages().lock(), conversation_id, timestamp)
    }

    /// Oldest first, so a reconnect re-sends in send order.
    pub fn pending_outgoing() -> Vec<MessageRow> {
        all(
            &core().db.messages().lock(),
            "SELECT * FROM messages WHERE outgoing = 1 AND status = 0 AND deleted = 0 ORDER BY id ASC",
            [],
            MessageRow::from_row,
        )
        .unwrap_or_default()
    }

    pub fn get_conversations() -> Vec<MessageRow> {
        all(
            &core().db.messages().lock(),
            "SELECT m.* FROM conversations c
             JOIN messages m ON m.id = (
                 SELECT id FROM messages WHERE conversation_id = c.id AND deleted = 0 ORDER BY id DESC LIMIT 1
             )
             ORDER BY m.id DESC",
            [],
            MessageRow::from_row,
        )
        .unwrap_or_default()
    }

    pub fn newest_incoming_dispatch(conversation_id: &[u8; 16]) -> Option<[u8; 16]> {
        let conn = core().db.messages().lock();
        conn.query_row(
            "SELECT dispatch_id FROM messages
             WHERE conversation_id = ?1 AND outgoing = 0 AND dispatch_id IS NOT NULL
             ORDER BY id DESC LIMIT 1",
            [conversation_id.as_slice()],
            |r| r.get(0),
        )
        .ok()
    }

    pub fn unread_counts() -> Vec<([u8; 16], u32)> {
        all(
            &core().db.messages().lock(),
            "SELECT m.conversation_id, COUNT(*) FROM incoming_receipts r
             CROSS JOIN messages m ON m.id = r.message_id
             WHERE r.is_read = 0 AND m.deleted = 0 AND m.system = 0
             GROUP BY m.conversation_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or_default()
    }
}

fn position_at_time(
    conn: &rusqlite::Connection, conversation: &[u8; 16], timestamp: u64,
) -> Result<Option<(String, Option<Vec<u8>>, u32)>> {
    Ok(one(
        conn,
        "SELECT m.id, m.dispatch_id, (SELECT COUNT(*) FROM messages n WHERE n.conversation_id = m.conversation_id AND n.deleted = 0 AND n.id > m.id) \
         FROM messages m WHERE m.conversation_id = ?1 AND m.deleted = 0 \
         ORDER BY CASE WHEN m.timestamp >= ?2 THEN 0 ELSE 1 END, \
                  CASE WHEN m.timestamp >= ?2 THEN m.timestamp END ASC, \
                  m.timestamp DESC, m.id ASC LIMIT 1",
        (conversation.as_slice(), timestamp),
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

pub fn dump_read_state_tx(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<(Vec<ReadRow>, Vec<MemberReadRow>)> {
    Ok((
        all(conn, "SELECT * FROM read_state", [], ReadRow::from_row)?,
        all(conn, "SELECT * FROM member_read_state", [], MemberReadRow::from_row)?,
    ))
}

pub fn import_read_state_tx(
    conn: &rusqlite::Connection, mine: &[ReadRow], theirs: &[MemberReadRow],
) -> Result<()> {
    for r in mine {
        conn.execute(
            "INSERT OR IGNORE INTO read_state (conversation_id, upto_dispatch_id) VALUES (?1, ?2)",
            (r.conversation_id.as_slice(), &r.upto_dispatch_id),
        )?;
    }
    for r in theirs {
        conn.execute(
            "INSERT OR IGNORE INTO member_read_state \
             (conversation_id, member_ipk, upto_dispatch_id) VALUES (?1, ?2, ?3)",
            (r.conversation_id.as_slice(), r.member_ipk.as_slice(), &r.upto_dispatch_id),
        )?;
    }
    Ok(())
}

impl Message {
    /// Snapshot only unseen, unread arrivals. Message ids avoid sender clock
    /// skew and multiple messages in the same timestamp second.
    pub fn pending_notification_ids(conversation_id: &[u8; 16]) -> Result<Vec<String>> {
        pending_notification_ids(&core().db.messages().lock(), conversation_id)
    }

    pub fn mark_notified(ids: &[String]) -> Result<()> {
        let mut conn = core().db.messages().lock();
        let tx = conn.transaction()?;
        mark_notified(&tx, ids)?;
        tx.commit()?;
        Ok(())
    }

    pub fn recent_incoming(conversation_id: &[u8; 16], limit: u32) -> Vec<MessageRow> {
        let mut rows = all(
            &core().db.messages().lock(),
            "SELECT * FROM messages \
             WHERE conversation_id = ?1 AND outgoing = 0 AND deleted = 0 \
             ORDER BY id DESC LIMIT ?2",
            (conversation_id.as_slice(), limit),
            MessageRow::from_row,
        )
        .unwrap_or_default();
        rows.reverse();
        rows
    }
}

fn pending_notification_ids(
    conn: &rusqlite::Connection, conversation_id: &[u8; 16],
) -> Result<Vec<String>> {
    Ok(all(
        conn,
        "SELECT m.id FROM messages m
         JOIN incoming_receipts r ON r.message_id = m.id
         WHERE m.conversation_id = ?1 AND m.outgoing = 0 AND m.deleted = 0 AND m.system = 0
           AND m.notification_seen = 0 AND r.is_read = 0",
        [conversation_id.as_slice()],
        |r| r.get(0),
    )?)
}

fn mark_notified(conn: &rusqlite::Connection, ids: &[String]) -> Result<()> {
    let mut stmt = conn.prepare(
        "UPDATE messages SET notification_seen = 1 WHERE id = ?1 AND notification_seen = 0",
    )?;
    for id in ids { stmt.execute([id])?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::data::conversation::Conversation;
    use crate::test_support::data::open;
    use crate::test_support::data::with_failing_trigger;

    fn receive_delete(
        conn: &mut Connection, conv: &[u8; 16], target: &[u8; 16], author: &[u8; 32],
    ) -> Result<Option<MessageRow>> {
        let tx = conn.transaction()?;
        let (row, _) = Message::receive_delete_tx(&tx, conv, target, author)?;
        tx.commit()?;
        Ok(row)
    }

    /// A deletion can outrun its post, even across MLS catch-up.
    #[test]
    fn early_deletion_is_author_scoped_durable_and_transactional() {
        let (conv, author, other, target) = ([0xD3; 16], [0xD4; 32], [0xD5; 32], [0xD6; 16]);
        let mut conn = open(crate::db::messages::migrate);
        assert!(receive_delete(&mut conn, &conv, &target, &author).unwrap().is_none());

        // A copy of the database knows it, so the ledger is not process memory.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messages.db");
        conn.execute("VACUUM INTO ?1", [path.to_str().unwrap()]).unwrap();
        let mut conn = Connection::open(&path).unwrap();

        let late = Message::save_incoming_tx(&conn, conv, author, &target, "late", 1, None);
        assert!(late.unwrap().is_none(), "the deleted post never lands");
        let theirs = Message::save_incoming_tx(&conn, conv, other, &target, "same id", 1, None);
        assert!(theirs.unwrap().is_some(), "no member can reserve another's dispatch id");
        assert!(receive_delete(&mut conn, &conv, &target, &author).unwrap().is_none());
        let shown = Message::get_messages_tx(&conn, &conv, 10, "").unwrap();
        assert_eq!(shown.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(), ["same id"]);

        let failed = [0xD7; 16];
        Message::save_incoming_tx(&conn, conv, author, &failed, "keep", 1, None).unwrap();
        with_failing_trigger(&mut conn, "messages", "UPDATE", |conn| {
            assert!(receive_delete(conn, &conv, &failed, &author).is_err());
        });
        let markers: u32 = conn
            .query_row(
                "SELECT COUNT(*) FROM message_deletions WHERE dispatch_id = ?1",
                [failed.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0, "a failed tombstone leaves no marker behind");

        Conversation::clear_history_tx(&conn, &conv).unwrap();
        let after = Message::save_incoming_tx(&conn, conv, author, &target, "again", 1, None);
        assert!(after.unwrap().is_none(), "the ledger outlives clearing history");
    }

    #[test]
    fn notification_acknowledgement_does_not_consume_a_racing_arrival() {
        let db = open(crate::db::messages::migrate);
        let (conversation, sender) = ([8; 16], [7; 32]);
        let insert = |did: [u8; 16], time| {
            Message::save_incoming_tx(&db, conversation, sender, &did, "message", time, None)
                .unwrap();
        };
        insert([1; 16], 100);
        let first = pending_notification_ids(&db, &conversation).unwrap();
        assert_eq!(first.len(), 1);
        insert([2; 16], 100);
        mark_notified(&db, &first).unwrap();
        let next = pending_notification_ids(&db, &conversation).unwrap();
        assert_eq!(next.len(), 1, "the arrival after the read is still pending");
        assert_ne!(next, first);
        mark_notified(&db, &next).unwrap();
        // An older sender clock and an already-delivered dispatch are distinct cases.
        insert([3; 16], 90);
        insert([2; 16], 100);
        assert_eq!(pending_notification_ids(&db, &conversation).unwrap().len(), 1);
        crate::data::receipts::read_tx(&db, &conversation, &[3; 16], 110).unwrap();
        assert!(pending_notification_ids(&db, &conversation).unwrap().is_empty());
    }

    #[test]
    fn dispatch_ids_strictly_increase_across_threads() {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| (0..1000).map(|_| next_dispatch_id()).collect::<Vec<_>>())
            })
            .collect();
        let mut every = Vec::new();
        for thread in threads {
            let ids = thread.join().unwrap();
            assert!(ids.windows(2).all(|w| w[0] < w[1]), "each caller sees a rising sequence");
            every.extend(ids);
        }
        let n = every.len();
        every.sort();
        every.dedup();
        assert_eq!(every.len(), n, "no two callers get the same id");
    }
}
