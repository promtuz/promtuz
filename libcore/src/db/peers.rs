use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug)]
pub struct ContactRow {
    /// Ed25519 identity public key
    pub ipk: [u8; 32],
    /// X25519 key exchange public key
    pub epk: [u8; 32],
    pub name: String,
    pub added_at: u64,
}

from_row!(ContactRow { ipk, epk, name, added_at });

const MIGRATION_ARRAY: &[M] = &[M::up(
    "CREATE TABLE contacts (
            ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32),
            epk BLOB NOT NULL CHECK(length(epk) = 32),
            name TEXT NOT NULL,
            added_at INTEGER NOT NULL
        );",
)];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static CONTACTS_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("contacts")).expect("db open failed");
    info!("DB: CONTACTS_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});
