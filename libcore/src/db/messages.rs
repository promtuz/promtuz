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
}

from_row!(MessageRow { id, peer_ipk, content, outgoing, timestamp, status });

const MIGRATION_ARRAY: &[M] = &[M::up(
    "CREATE TABLE messages (
            id TEXT PRIMARY KEY,
            peer_ipk BLOB NOT NULL CHECK(length(peer_ipk) = 32),
            content TEXT NOT NULL,
            outgoing INTEGER NOT NULL,
            timestamp INTEGER NOT NULL,
            status INTEGER NOT NULL DEFAULT 0
        );
    CREATE INDEX idx_messages_peer ON messages(peer_ipk, id DESC);",
)];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static MESSAGES_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("messages")).expect("db open failed");
    info!("DB: MESSAGES_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
