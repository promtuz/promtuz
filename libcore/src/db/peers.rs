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
    pub ipk:           [u8; 32],
    pub name:          String,
    pub added_at:      u64,
    /// 32-byte MLS GroupId of the implicit 1:1 group with this contact, or
    /// `None` if no group has been created yet (lazy-created on first send).
    ///
    /// Replaces the v2-era `(epk, enc_esk)` columns and
    /// `Contact::shared_key()` derivation. See `misc/specs/MLS.md` §11.3.
    pub mls_group_id:  Option<[u8; 32]>,
}

from_row!(ContactRow { ipk, name, added_at, mls_group_id });

/// Hard cutover (`misc/specs/MLS.md` §11.3): drop the v2 shared-key
/// columns (`epk`, `enc_esk`) and add `mls_group_id`.
///
/// **Pre-1.0 hard cutover note**: any pre-existing rows survive the
/// migration because we wipe and recreate the `contacts` table. Pre-1.0
/// nobody has prod data, so accepting "every existing contact must be
/// re-added" is acceptable. Post-migration any conversation that needs
/// to send a message lazy-creates an MLS group on first dispatch.
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
    // Schema cutover. Drop v2 shared-key columns; add `mls_group_id`.
    // Pre-1.0 hard cutover: no in-place column rewrite, we drop and
    // recreate. See `misc/specs/MLS.md` §11.3.
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
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static CONTACTS_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("contacts")).expect("db open failed");
    info!("DB: CONTACTS_DB CONNECTED");

    PRAGMA!(conn, MIGRATIONS);

    Mutex::new(conn)
});

#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory db");
    PRAGMA!(conn, MIGRATIONS);
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The schema migration applies cleanly to a fresh DB.
    /// Old `(epk, enc_esk)` columns must be gone; new `mls_group_id`
    /// column must be present.
    #[test]
    fn migration_drops_v2_columns_and_adds_mls_group_id() {
        let conn = open_in_memory();
        // Schema introspection: the contacts table has only
        // (ipk, name, added_at, mls_group_id) post-migration.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(contacts)")
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>("name"))
            .expect("query")
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(cols.len(), 4, "expected 4 columns post-migration, got {cols:?}");
        assert!(cols.contains(&"ipk".to_string()));
        assert!(cols.contains(&"name".to_string()));
        assert!(cols.contains(&"added_at".to_string()));
        assert!(cols.contains(&"mls_group_id".to_string()));
        assert!(!cols.contains(&"epk".to_string()), "v2 epk column must be dropped");
        assert!(
            !cols.contains(&"enc_esk".to_string()),
            "v2 enc_esk column must be dropped"
        );
    }

    /// Insert a row with NULL `mls_group_id` and read it back.
    /// Confirms the nullable column round-trips.
    #[test]
    fn null_mls_group_id_round_trips() {
        let conn = open_in_memory();
        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id) VALUES (?1, ?2, ?3, NULL)",
            ([0x42u8; 32], "alice", 1234u64),
        )
        .expect("insert");

        let row: ContactRow = conn
            .query_row("SELECT * FROM contacts", [], ContactRow::from_row)
            .expect("query");
        assert_eq!(row.ipk, [0x42; 32]);
        assert_eq!(row.name, "alice");
        assert_eq!(row.added_at, 1234);
        assert!(row.mls_group_id.is_none());
    }

    /// Insert a row with a populated `mls_group_id` and read it back.
    #[test]
    fn populated_mls_group_id_round_trips() {
        let conn = open_in_memory();
        let gid = [0xAAu8; 32];
        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id) VALUES (?1, ?2, ?3, ?4)",
            ([0x42u8; 32], "bob", 1234u64, gid),
        )
        .expect("insert");

        let row: ContactRow = conn
            .query_row("SELECT * FROM contacts", [], ContactRow::from_row)
            .expect("query");
        assert_eq!(row.mls_group_id, Some(gid));
    }
}
