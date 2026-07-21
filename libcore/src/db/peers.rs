use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::PRAGMA;
use super::macros::from_row;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ContactRow {
    /// Their Ed25519 identity public key
    pub ipk:           [u8; 32],
    pub name:          String,
    pub added_at:      u64,
    /// 32-byte MLS GroupId of the implicit 1:1 group with this contact, or
    /// `None` if no group has been created yet (lazy-created on first send).
    ///
    /// Replaces the v2-era `(epk, enc_esk)` columns and
    /// `Contact::shared_key()` derivation.
    pub mls_group_id:  Option<[u8; 32]>,
    /// Pairing state: 0 = pending (welcome published, not yet confirmed),
    /// 1 = paired (proven by an inbound MLS message), 2 = rejected. Legacy
    /// rows default to paired — they already work.
    pub status:        u8,
    /// Why the pair was rejected (a `DECLINE_*` reason), when `status = 2`.
    pub reject_reason: Option<u8>,
}

from_row!(ContactRow { ipk, name, added_at, mls_group_id, status, reject_reason });

/// Hard cutover: drop the v2 shared-key columns (`epk`, `enc_esk`) and
/// add `mls_group_id`.
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
    // recreate.
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
    // Pairing state machine (PAIRING.md). Default paired: legacy contacts
    // already have a working group.
    M::up("ALTER TABLE contacts ADD COLUMN status INTEGER NOT NULL DEFAULT 1;"),
    M::up("ALTER TABLE contacts ADD COLUMN reject_reason INTEGER;"),
    // Heal contacts falsely rejected by an unconditional pair-decline (before
    // mark_rejected gated on group-presence): a live MLS group is proof of a
    // working pair, so restore PAIRED and clear the stale reason.
    M::up("UPDATE contacts SET status = 1, reject_reason = NULL WHERE status = 2 AND mls_group_id IS NOT NULL;"),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static CONTACTS_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("contacts")).expect("db open failed");
    PRAGMA!(conn, MIGRATIONS);
    super::register_change_hook(&conn, &["contacts"]);

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
        assert_eq!(cols.len(), 6, "expected 6 columns post-migration, got {cols:?}");
        assert!(cols.contains(&"ipk".to_string()));
        assert!(cols.contains(&"name".to_string()));
        assert!(cols.contains(&"added_at".to_string()));
        assert!(cols.contains(&"mls_group_id".to_string()));
        assert!(cols.contains(&"status".to_string()));
        assert!(cols.contains(&"reject_reason".to_string()));
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

    /// Pairing status transitions (PAIRING.md): a fresh row defaults to paired
    /// (legacy safety); save_pending's insert sets pending; mark_paired's
    /// guarded UPDATE flips pending→paired once and never downgrades a paired.
    #[test]
    fn status_flip_is_one_way_and_guarded() {
        let conn = open_in_memory();
        let ipk = [7u8; 32];
        let status = |c: &Connection| -> u8 {
            c.query_row("SELECT status FROM contacts WHERE ipk = ?1", [ipk.as_slice()], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap() as u8
        };
        // save_pending shape: insert as pending (0).
        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id, status) VALUES (?1,?2,?3,NULL,0)",
            (ipk.as_slice(), "b", 1u64),
        )
        .unwrap();
        assert_eq!(status(&conn), 0, "saved pending");

        // mark_paired shape: pending → paired, once.
        let flip = |c: &Connection| {
            c.execute("UPDATE contacts SET status = 1 WHERE ipk = ?1 AND status = 0", [ipk.as_slice()])
                .unwrap()
        };
        assert_eq!(flip(&conn), 1, "flips pending → paired");
        assert_eq!(status(&conn), 1);
        assert_eq!(flip(&conn), 0, "idempotent — already paired, no rows touched");

        // A paired row is never downgraded by a re-pair's ON CONFLICT (name only).
        conn.execute(
            "INSERT INTO contacts (ipk, name, added_at, mls_group_id, status) VALUES (?1,?2,?3,NULL,0) \
             ON CONFLICT(ipk) DO UPDATE SET name = excluded.name",
            (ipk.as_slice(), "b2", 2u64),
        )
        .unwrap();
        assert_eq!(status(&conn), 1, "re-pair must not downgrade a live paired contact");
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
