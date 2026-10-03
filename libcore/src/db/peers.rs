use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::from_row;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ContactRow {
    pub ipk:           [u8; 32],
    pub name:          String,
    pub added_at:      u64,
    /// The 1:1 MLS group, `None` until the first send creates it.
    pub mls_group_id:  Option<[u8; 32]>,
    /// 0 = pending (Welcome sent, unconfirmed), 1 = paired (proven by an inbound MLS message),
    /// 2 = rejected, 3 = an unaccepted message request.
    pub status:        u8,
    /// Why the pair was rejected (a `DECLINE_*` reason), when `status = 2`.
    pub reject_reason: Option<u8>,
}

from_row!(ContactRow { ipk, name, added_at, mls_group_id, status, reject_reason });

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        "CREATE TABLE contacts (
            ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32),
            epk BLOB NOT NULL CHECK(length(epk) = 32),
            enc_esk BLOB NOT NULL,
            name TEXT NOT NULL,
            added_at INTEGER NOT NULL
        );",
    ),
    M::up(
        r#"
        DROP TABLE contacts;
        CREATE TABLE contacts (
            ipk BLOB PRIMARY KEY CHECK(length(ipk) = 32),
            name TEXT NOT NULL,
            added_at INTEGER NOT NULL,
            mls_group_id BLOB CHECK(mls_group_id IS NULL OR length(mls_group_id) = 32)
        );
        "#,
    ),
    // Default paired: contacts from before pairing already have a working group.
    M::up("ALTER TABLE contacts ADD COLUMN status INTEGER NOT NULL DEFAULT 1;"),
    M::up("ALTER TABLE contacts ADD COLUMN reject_reason INTEGER;"),
    // A rejected contact with a live MLS group is paired; restore it.
    M::up("UPDATE contacts SET status = 1, reject_reason = NULL WHERE status = 2 AND mls_group_id IS NOT NULL;"),
];
pub(super) const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub fn migrate(conn: &mut Connection) {
    super::prepare(conn, &MIGRATIONS);
}
