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
    /// Their Ed25519 identity public key
    pub ipk: [u8; 32],
    /// Their X25519 public key for this friendship
    pub epk: [u8; 32],
    /// Our X25519 secret key for this friendship (encrypted via KeyManager)
    pub enc_esk: Vec<u8>,
    pub name: String,
    pub added_at: u64,
}

from_row!(ContactRow { ipk, epk, enc_esk, name, added_at });

const MIGRATION_ARRAY: &[M] = &[M::up(
    "CREATE TABLE contacts (
            ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32),
            epk BLOB NOT NULL CHECK(length(epk) = 32),
            enc_esk BLOB NOT NULL,
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
