use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug)]
pub struct IdentityRow {
    pub id: u8,
    pub ipk: [u8; 32],
    pub enc_isk: Vec<u8>,
    // pub vfk: [u8; 32],
    // pub enc_vsk: Vec<u8>,
    /// Unix timestamp in milliseconds
    pub created_at: u64,
    pub name: String,
    /// Our profile picture as AVIF, as last set. `None` when we have none.
    pub avatar: Option<Vec<u8>>,
    pub avatar_revision: u64,
}

from_row!(IdentityRow { id, ipk, enc_isk, created_at, name, avatar, avatar_revision });

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        "CREATE TABLE identity (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            ipk BLOB NOT NULL CHECK(length(ipk) = 32),
            enc_isk BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            name TEXT NOT NULL
        );",
    ),
    // Invite ids already redeemed. Rows are pruned once the invite can no
    // longer be presented at all, so this stays bounded by the number of
    // invites minted in one acceptance window.
    M::up(
        "CREATE TABLE spent_invite (
            id           BLOB PRIMARY KEY CHECK(length(id) = 16),
            unusable_at_ms INTEGER NOT NULL
        );",
    ),
    // The picture beside the name. It lives with the identity for the reason
    // the name does: it is what we tell people about ourselves, and a restore
    // should bring it back with the rest of the profile.
    M::up("ALTER TABLE identity ADD COLUMN avatar BLOB;"),
    M::up("ALTER TABLE identity ADD COLUMN avatar_revision INTEGER NOT NULL DEFAULT 0;"),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static IDENTITY_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("identity")).expect("db open failed");
    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});

#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("identity DB");
    MIGRATIONS.to_latest(&mut conn).expect("identity migrations");
    conn
}
