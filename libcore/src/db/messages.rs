use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;
use serde::Serialize;

use crate::db::utils::ulid::ULID;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug, Clone, Serialize)]
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
    pub dispatch_id: Vec<u8>,
}

from_row!(MessageRow { id, peer_ipk, content, outgoing, timestamp, status, dispatch_id });

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
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static MESSAGES_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("messages")).expect("db open failed");
    info!("DB: MESSAGES_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});

#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    PRAGMA!(conn, MIGRATIONS);
    conn
}
