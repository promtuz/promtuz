use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;
use serde::Deserialize;
use serde::Serialize;

use crate::db::utils::ulid::ULID;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    /// ULID string (26 chars, time-sortable)
    pub id: ULID,
    /// The other party's IPK (sender if incoming, recipient if outgoing)
    #[serde(with = "serde_bytes")]
    pub peer_ipk: [u8; 32],
    pub content: String,
    /// 1 = sent by us, 0 = received
    pub outgoing: bool,
    pub timestamp: u64,
    /// 0 = pending, 1 = sent, 2 = failed
    pub status: u8,
    /// Sender-minted monotonic id (16 bytes); NULL on legacy rows.
    /// Cross-device dedup + convergence key — the ULID `id` stays the
    /// row PK / ordering key.
    pub dispatch_id: Option<Vec<u8>>,
    /// Sender edited this message's text after sending.
    pub edited: bool,
    /// Tombstoned by delete-for-everyone; `content` is cleared.
    pub deleted: bool,
    /// dispatch_id of the message this one quotes (reply). NULL = plain text.
    pub reply_to: Option<Vec<u8>>,
}

from_row!(MessageRow { id, peer_ipk, content, outgoing, timestamp, status, dispatch_id, edited, deleted, reply_to });

/// One emoji reaction on a message. Keyed by `reactor` (an IPK, not a
/// me/them bool) so a multi-member group attributes each reaction to its
/// author. `peer_ipk` is the conversation scope (the 1:1 peer today; a
/// group id once group chats exist). `dispatch_id` names the reacted message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRow {
    #[serde(with = "serde_bytes")]
    pub peer_ipk: [u8; 32],
    #[serde(with = "serde_bytes")]
    pub dispatch_id: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub reactor: [u8; 32],
    pub emoji: String,
    pub timestamp: u64,
}

from_row!(ReactionRow { peer_ipk, dispatch_id, reactor, emoji, timestamp });

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        "CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            content TEXT NOT NULL,
            outgoing INTEGER NOT NULL,
            timestamp INTEGER NOT NULL,
            status INTEGER NOT NULL DEFAULT 0
        );
    CREATE INDEX idx_messages_peer ON messages(peer_ipk, id DESC);",
    ),
    M::up("ALTER TABLE messages ADD COLUMN dispatch_id BLOB;"),
    // Partial unique index: legacy rows have NULL dispatch_id and must not collide.
    M::up(
        "CREATE UNIQUE INDEX idx_messages_dedup ON messages(peer_ipk, dispatch_id) WHERE dispatch_id IS NOT NULL;",
    ),
    M::up("ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0;"),
    M::up("ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;"),
    M::up(
        "CREATE TABLE reactions (
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            reactor BLOB NOT NULL CHECK(length(reactor) = 32),
            emoji TEXT NOT NULL,
            timestamp INTEGER NOT NULL,
            PRIMARY KEY (peer_ipk, dispatch_id, reactor, emoji)
        ) WITHOUT ROWID;
    CREATE INDEX idx_reactions_msg ON reactions(peer_ipk, dispatch_id);",
    ),
    M::up("ALTER TABLE messages ADD COLUMN reply_to BLOB;"),
    // Delivery dedup ledger: a dispatch we already decrypted must never be
    // re-decrypted (the MLS ratchet consumed its key → SecretReuseError).
    // Redelivery from the other K-home relays on reconnect is the trigger.
    M::up(
        "CREATE TABLE seen_dispatch (
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            dispatch_id BLOB NOT NULL,
            seen_at INTEGER NOT NULL,
            PRIMARY KEY (peer_ipk, dispatch_id)
        ) WITHOUT ROWID;",
    ),
    // Local read high-water-mark per peer: the newest incoming dispatch_id the
    // user has read. Drives the home-list unread count; mark_read upserts it.
    M::up(
        "CREATE TABLE read_state (
            peer_ipk BLOB PRIMARY KEY CHECK(length(peer_ipk) = 32),
            upto_dispatch_id BLOB NOT NULL
        ) WITHOUT ROWID;",
    ),
    // Per-message media metadata (Image inline bytes / Attachment thumb +
    // file_id), keyed to the message it belongs to. The caption stays on
    // messages.content; this only holds the media side of the payload.
    M::up(
        "CREATE TABLE message_media (
            peer_ipk    BLOB NOT NULL,
            dispatch_id BLOB NOT NULL,
            kind        INTEGER NOT NULL,
            group_id    BLOB,
            mime        TEXT NOT NULL,
            name        TEXT NOT NULL DEFAULT '',
            size        INTEGER NOT NULL DEFAULT 0,
            width       INTEGER NOT NULL DEFAULT 0,
            height      INTEGER NOT NULL DEFAULT 0,
            blob        BLOB,
            thumb       BLOB,
            file_id     BLOB,
            PRIMARY KEY (peer_ipk, dispatch_id)
        );",
    ),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static MESSAGES_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("messages")).expect("db open failed");
    info!("DB: MESSAGES_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);
    super::register_change_hook(&conn, &["messages", "reactions", "message_media"]);

    Mutex::new(conn)
});

#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    PRAGMA!(conn, MIGRATIONS);
    conn
}
