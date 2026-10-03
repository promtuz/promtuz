use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::from_row;

#[derive(Debug)]
pub struct IdentityRow {
    pub id: u8,
    pub ipk: [u8; 32],
    pub enc_isk: Vec<u8>,
    /// Unix timestamp in milliseconds
    pub created_at: u64,
    pub name: String,
    /// Our profile picture as AVIF, as last set. `None` when we have none.
    pub avatar: Option<Vec<u8>>,
    pub avatar_revision: u64,
    pub bio: String,
    pub profile_revision: u64,
}

from_row!(IdentityRow { id, ipk, enc_isk, created_at, name, avatar, avatar_revision, bio, profile_revision });

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
    // Redeemed invite ids, pruned once the invite can no longer be presented.
    M::up(
        "CREATE TABLE spent_invite (
            id           BLOB PRIMARY KEY CHECK(length(id) = 16),
            unusable_at_ms INTEGER NOT NULL
        );",
    ),
    // The avatar lives with the identity, so a restore brings it back with the profile.
    M::up("ALTER TABLE identity ADD COLUMN avatar BLOB;"),
    M::up("ALTER TABLE identity ADD COLUMN avatar_revision INTEGER NOT NULL DEFAULT 0;"),
    M::up("ALTER TABLE identity ADD COLUMN bio TEXT NOT NULL DEFAULT ''; ALTER TABLE identity ADD COLUMN profile_revision INTEGER NOT NULL DEFAULT 0;"),
];
pub(super) const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub fn migrate(conn: &mut Connection) {
    super::prepare(conn, &MIGRATIONS);
}
