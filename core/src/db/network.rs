use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use crate::PRAGMA;
use crate::db::db;

#[derive(Debug)]
pub struct RelayRow {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub last_avg_latency: Option<u64>,
    pub last_seen_unix: u64,
    pub last_connect: Option<u64>,
    pub last_version: u16,
    pub reputation: i16,
}

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        "CREATE TABLE relays (
              id TEXT PRIMARY KEY,
              host TEXT NOT NULL,
              port INTEGER NOT NULL CHECK(port > 0 AND port <= 65535),
              last_avg_latency INTEGER CHECK(last_avg_latency >= 0),
              last_seen_unix INTEGER NOT NULL,
              last_connect INTEGER,
              last_version INTEGER NOT NULL,
              reputation INTEGER NOT NULL DEFAULT 0
          );",
    ),
    M::up("CREATE INDEX idx_relays_reputation_seen ON relays(reputation DESC, last_seen DESC)"),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static NETWORK_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(db("network")).expect("db open failed");
    info!("DB: NETWORK_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
