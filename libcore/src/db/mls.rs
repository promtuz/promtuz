//! SQLite database for the MLS storage provider.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE mls_storage (
//!   group_id BLOB NOT NULL,
//!   key_tag  INTEGER NOT NULL,
//!   sub_key  BLOB NOT NULL,
//!   value    BLOB NOT NULL,
//!   PRIMARY KEY (group_id, key_tag, sub_key)
//! );
//! ```
//!
//! `group_id` is the CBOR-serialised openmls `GroupId` (or an empty BLOB
//! for entries that aren't scoped to a specific group, e.g. signature key
//! pairs, key packages, PSKs). `key_tag` distinguishes the openmls
//! `Entity` variant; `sub_key` is the CBOR-serialised secondary key
//! (proposal ref, leaf index, encryption pubkey, …) — empty when the
//! `(group_id, key_tag)` pair is sufficient.
//!
//! The 1-byte tag namespace is defined in `mls::storage::tags`.
//!
//! No domain types are exposed from here — the provider talks to the
//! connection directly. We do export [`apply_mls_migrations`] so test
//! fixtures can spin up a fresh `:memory:` connection without going
//! through the on-disk singleton.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;

use super::macros::PRAGMA;

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        r#"--sql
        CREATE TABLE mls_storage (
            group_id BLOB NOT NULL,
            key_tag  INTEGER NOT NULL,
            sub_key  BLOB NOT NULL,
            value    BLOB NOT NULL,
            PRIMARY KEY (group_id, key_tag, sub_key)
        );
        CREATE INDEX idx_mls_storage_group ON mls_storage(group_id);
    "#,
    ),
    // Out-of-order epoch buffer.
    //
    // Stores buffered MLS messages received ahead of the current group
    // epoch (e.g. an Application at epoch=N+1 arriving before its
    // load-bearing Commit). On a commit-merge that advances the local
    // epoch, [`mls::epoch_catchup::EpochCatchupBuffer::drain_when_ready`]
    // re-scans this table for newly-processable rows.
    //
    // Bounded by `MAX_EPOCH_AHEAD_BUFFER` (512) entries per `group_id`.
    // On overflow we drop the **newest** entry (older entries are closer
    // to the current epoch and more likely to be the load-bearing commit).
    //
    // PK is `(group_id, dispatch_id)` so a duplicate insert (replayed
    // dispatch) is a no-op.
    M::up(
        r#"--sql
        CREATE TABLE mls_epoch_ahead (
            group_id        BLOB    NOT NULL,
            epoch           INTEGER NOT NULL,
            dispatch_id     BLOB    NOT NULL,
            msg_blob        BLOB    NOT NULL,
            received_at_ms  INTEGER NOT NULL,
            PRIMARY KEY (group_id, dispatch_id)
        );
        CREATE INDEX idx_mls_epoch_ahead_group_epoch
            ON mls_epoch_ahead(group_id, epoch);
        CREATE INDEX idx_mls_epoch_ahead_received
            ON mls_epoch_ahead(group_id, received_at_ms);
    "#,
    ),
    // KeyPackage stash bookkeeping.
    //
    // Tracks promtuz's view of which `kp_ref`s the client has minted
    // and not yet seen consumed (via a Welcome). The actual KP
    // private-key material lives in `mls_storage` keyed by
    // `tags::KEY_PACKAGE` (openmls owns it via its `StorageProvider`);
    // this table only records the promtuz-side metadata needed to
    // schedule rotation and refill.
    //
    // Columns:
    // - `kp_ref`: RFC 9420 §5.2 KeyPackageRef (label-prefixed SHA-256
    //   over the TLS-encoded KP under suite 0x0003). PRIMARY KEY
    //   because it's globally unique.
    // - `generated_at_ms`: when the client minted this KP. Drives the
    //   anti-pinning rotation cadence (`KP_SCHEDULED_ROTATION_MS`).
    // - `expires_at_ms`: when the KP's lifetime extension elapses.
    //   Used to prune ageing rows independent of consumption signal.
    // - `consumed`: 0/1 flag; flips to 1 when libcore observes a
    //   Welcome consuming this KP. The welcome-receipt path hooks
    //   into this; until then we only write 0.
    M::up(
        r#"--sql
        CREATE TABLE mls_keypackage_stash (
            kp_ref          BLOB PRIMARY KEY,
            generated_at_ms INTEGER NOT NULL,
            expires_at_ms   INTEGER NOT NULL,
            consumed        INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_mls_kp_stash_unconsumed
            ON mls_keypackage_stash(consumed, expires_at_ms);
    "#,
    ),
    // List-typed entities (OWN_LEAF_NODES = 0x02,
    // PROPOSAL_QUEUE_REFS = 0x04) used to be stored as a single row
    // per `(group_id, key_tag)` carrying a CBOR-encoded
    // `Vec<Vec<u8>>` — every append decoded the full list, pushed
    // one element, re-encoded, and re-wrote the entire blob (O(N²)
    // amortised, multi-MB CBOR work for 50 proposals in a 256-member
    // group).
    //
    // The new layout stores each list element as a separate row
    // keyed on `(group_id, key_tag, sub_key = u64_be(list_index))`.
    // Append is one INSERT; read is `SELECT … ORDER BY sub_key`.
    //
    // Pre-1.0 hard cutover: nuke any pre-existing list-typed rows.
    // Users with in-progress groups must re-bootstrap (acceptable
    // pre-1.0).
    M::up(
        r#"--sql
        DELETE FROM mls_storage WHERE key_tag IN (2, 4);
    "#,
    ),
    // Sidecar table to make `check_budget` a single-row lookup
    // instead of `SELECT SUM(length(value))` (full per-group scan on
    // every put). openmls performs 10+ writes per
    // Commit; the SUM-on-every-put was the single largest CPU
    // hotspot in the storage layer.
    //
    // `total_bytes` is maintained transactionally inside `put`
    // (delta = new_value_len - prev_row_len, applied with
    // INSERT OR REPLACE arithmetic). `delete_one` / `delete_by_tag`
    // do the negative-delta side.
    M::up(
        r#"--sql
        CREATE TABLE mls_group_size (
            group_id    BLOB PRIMARY KEY,
            total_bytes INTEGER NOT NULL
        );
    "#,
    ),
    // Carry the full serialized `KeyPackageRecord` alongside the
    // bookkeeping so the client can republish its stash to a relay that
    // lost our KP without re-minting (see `keypackage::unconsumed_records`).
    //
    // The DELETE wipes only stash BOOKKEEPING rows (legacy rows have no
    // `record_blob`); the openmls KP *bundles* stay in `mls_storage`, so
    // any already-published KP a peer still holds stays usable until it
    // expires. The client re-mints a fresh full stash (now with
    // `record_blob`) on its next connect — this is what unsticks an
    // already-stuck client.
    M::up(
        r#"--sql
        ALTER TABLE mls_keypackage_stash ADD COLUMN record_blob BLOB;
        DELETE FROM mls_keypackage_stash;
    "#,
    ),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

/// Apply MLS migrations to a caller-supplied connection.
///
/// Used by both the global [`MLS_DB`] singleton and by tests that build
/// `Connection::open_in_memory()` instances. Sets the same PRAGMAs the
/// rest of libcore uses (WAL, foreign keys, etc.).
pub fn apply_mls_migrations(conn: &mut Connection) {
    // PRAGMA! macro expands `&mut $conn`; the binding must be `mut`.
    let mut conn = conn;
    PRAGMA!(conn, MIGRATIONS);
}

/// Process-global MLS SQLite connection. All libcore call sites share
/// this single `Arc<Mutex<Connection>>` — the in-process mutex is
/// engaged for every read/write, eliminating the connection-open
/// overhead and closing the inter-handle race window where two
/// `sendMessage` calls could interleave their MLS state mutations
/// through separate SQLite connections.
pub static MLS_DB: Lazy<std::sync::Arc<Mutex<Connection>>> = Lazy::new(|| {
    let mut conn = Connection::open(super::db("mls")).expect("db open failed");
    apply_mls_migrations(&mut conn);

    std::sync::Arc::new(Mutex::new(conn))
});

/// Return the process-global MLS DB handle. Returns a clone of the
/// [`MLS_DB`] static's `Arc` instead of opening a fresh handle each
/// call. Closes the prior race window where two concurrent dispatches
/// could mutate MLS state through separate SQLite connections, and
/// removes the per-`sendMessage` connection-open cost.
///
/// Used by the messaging API and the QUIC server for stash +
/// epoch-ahead buffer construction.
pub fn stash_db_handle() -> std::sync::Arc<parking_lot::Mutex<Connection>> {
    std::sync::Arc::clone(&MLS_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Apply the MLS migrations to a fresh `:memory:` connection.
    /// Asserts the table and index exist post-migration.
    #[test]
    fn migration_applies_to_empty_database() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_mls_migrations(&mut conn);

        let table_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='mls_storage'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(table_exists, 1, "mls_storage table missing");

        let index_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_mls_storage_group'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(index_exists, 1, "idx_mls_storage_group index missing");
    }

    /// Applying migrations a second time on the same DB must be a no-op
    /// (rusqlite_migration tracks via user_version). Also verifies that
    /// pre-existing rows survive a re-run (idempotency on populated DB).
    #[test]
    fn migration_is_idempotent_on_populated_database() {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        apply_mls_migrations(&mut conn);

        conn.execute(
            "INSERT INTO mls_storage(group_id, key_tag, sub_key, value) \
             VALUES (?1, ?2, ?3, ?4)",
            (vec![1u8, 2, 3], 7i64, vec![] as Vec<u8>, vec![0xAAu8; 32]),
        )
        .expect("insert");

        // Re-run migrations.
        apply_mls_migrations(&mut conn);

        let count: i64 = conn
            .query_row("SELECT count(*) FROM mls_storage", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, 1, "row should survive a re-run of migrations");
    }
}
