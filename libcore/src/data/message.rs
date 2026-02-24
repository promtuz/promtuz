use anyhow::Result;
use ulid::Ulid;

use crate::db::messages::MESSAGES_DB;
use crate::db::messages::MessageRow;
use crate::utils::systime;

/// Message status constants
pub const STATUS_PENDING: u8 = 0;
pub const STATUS_SENT: u8 = 1;
pub const STATUS_FAILED: u8 = 2;

#[derive(Debug, Clone)]
pub struct Message {
    pub inner: MessageRow,
}

impl Message {
    /// Save an outgoing message (status = pending until relay confirms).
    pub fn save_outgoing(peer_ipk: [u8; 32], content: &str) -> Result<Self> {
        let id = Ulid::new();
        let timestamp = systime().as_secs();
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status) VALUES (?1, ?2, ?3, 1, ?4, ?5)",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_PENDING),
        )?;

        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                peer_ipk,
                content: content.to_string(),
                outgoing: true,
                timestamp,
                status: STATUS_PENDING,
            },
        })
    }

    /// Save an incoming (received) message.
    pub fn save_incoming(peer_ipk: [u8; 32], content: &str, timestamp: u64) -> Result<Self> {
        let id = Ulid::new();
        let conn = MESSAGES_DB.lock();
        conn.execute(
            "INSERT INTO messages (id, peer_ipk, content, outgoing, timestamp, status) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            (&id.to_string(), peer_ipk, content, timestamp, STATUS_SENT),
        )?;

        Ok(Self {
            inner: MessageRow {
                id: id.into(),
                peer_ipk,
                content: content.to_string(),
                outgoing: false,
                timestamp,
                status: STATUS_SENT,
            },
        })
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
