use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use crate::PRAGMA;
use crate::db::db;

#[derive(Debug)]
pub struct IdentityRow {
    pub id: u8,
    pub ipk: [u8; 32],
    pub enc_isk: Vec<u8>,
    pub vfk: [u8; 32],
    pub enc_vsk: Vec<u8>,
    /// Unix timestamp in milliseconds
    pub created_at: u64,
    pub name: String,
}

const MIGRATION_ARRAY: &[M] = &[M::up(
    "CREATE TABLE identity (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            ipk BLOB NOT NULL CHECK(length(ipk) = 32),
            enc_isk BLOB NOT NULL,
            vfk BLOB NOT NULL CHECK(length(vfk) = 32),
            enc_vsk BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            name TEXT NOT NULL
        );",
)];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static IDENTITY_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(db("identity")).expect("db open failed");
    info!("DB: IDENTITY_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
