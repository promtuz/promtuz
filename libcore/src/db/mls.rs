//! The MLS database: openmls storage (see `mls::storage`) and promtuz's MLS bookkeeping.

use rusqlite::Connection;
use rusqlite_migration::M;
use rusqlite_migration::Migrations;


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
    // Messages ahead of the group's epoch. Past `MAX_EPOCH_AHEAD_BUFFER` per group the newest is
    // dropped, since older rows are likelier the commit that unblocks the rest.
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
    // KeyPackages we minted. `consumed` is 0 unused, 1 taken by a Welcome, 2 retired by rotation.
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
    // Drops list slots stored as one blob; they now keep one row per element.
    M::up(
        r#"--sql
        DELETE FROM mls_storage WHERE key_tag IN (2, 4);
    "#,
    ),
    // Per-group byte totals, kept in step by every storage write so the budget check is one lookup.
    M::up(
        r#"--sql
        CREATE TABLE mls_group_size (
            group_id    BLOB PRIMARY KEY,
            total_bytes INTEGER NOT NULL
        );
    "#,
    ),
    // Full records, so the stash can be republished without minting. The DELETE clears only the
    // bookkeeping; openmls keeps the bundles, so KeyPackages peers already hold stay usable.
    M::up(
        r#"--sql
        ALTER TABLE mls_keypackage_stash ADD COLUMN record_blob BLOB;
        DELETE FROM mls_keypackage_stash;
    "#,
    ),
    // Origin-relay acceptance time, so a buffered message keeps its send date. 0 on older rows,
    // where readers fall back to `received_at_ms`.
    M::up(
        r#"--sql
        ALTER TABLE mls_epoch_ahead ADD COLUMN accepted_at_ms INTEGER NOT NULL DEFAULT 0;
    "#,
    ),
    // Keep the signed dispatch identity separate from the buffer's dedup key.
    // NULL preserves old entries whose original identity was never recorded.
    M::up(
        r#"--sql
        ALTER TABLE mls_epoch_ahead ADD COLUMN original_dispatch_id BLOB
            CHECK(original_dispatch_id IS NULL OR length(original_dispatch_id) = 16);
        ALTER TABLE mls_epoch_ahead ADD COLUMN dispatch_sender BLOB
            CHECK(dispatch_sender IS NULL OR length(dispatch_sender) = 32);
    "#,
    ),
    M::up(
        r#"
        CREATE TABLE mls_branches (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            parent BLOB, epoch INTEGER NOT NULL, rank INTEGER NOT NULL,
            commit_hash BLOB NOT NULL, commit_blob BLOB,
            snapshot BLOB NOT NULL, change_blob BLOB, proof BLOB,
            archived_at INTEGER,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            PRIMARY KEY(group_id, branch)
        );
        CREATE INDEX mls_branches_parent ON mls_branches(group_id, parent, rank DESC, commit_hash);
        CREATE TABLE mls_recovery_roots (
            group_id BLOB PRIMARY KEY, branch BLOB NOT NULL
        );
        CREATE TABLE mls_join_history (
            group_id BLOB PRIMARY KEY, inviter BLOB NOT NULL, history BLOB NOT NULL
        );
        CREATE TABLE mls_recovery_retries (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            PRIMARY KEY(group_id,branch)
        );
        CREATE TABLE mls_replay (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id BLOB NOT NULL, dispatch_id BLOB NOT NULL,
            branch BLOB NOT NULL, payload BLOB NOT NULL, recipients BLOB NOT NULL,
            wake INTEGER NOT NULL, kind INTEGER NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(group_id, dispatch_id)
        );
        CREATE TABLE mls_dispatch_jobs (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            recipient BLOB NOT NULL, dispatch_id BLOB NOT NULL,
            kind INTEGER NOT NULL, frame BLOB NOT NULL,
            PRIMARY KEY(recipient, dispatch_id)
        );
        CREATE TABLE mls_dispatch_ids (
            group_id BLOB NOT NULL, dispatch_id BLOB PRIMARY KEY,
            logical_id BLOB NOT NULL
        );
        CREATE TABLE mls_branch_inbox (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            sender BLOB NOT NULL, dispatch_id BLOB NOT NULL,
            accepted_at_ms INTEGER NOT NULL, envelope BLOB NOT NULL,
            received_at INTEGER NOT NULL DEFAULT (unixepoch()),
            PRIMARY KEY(group_id, sender, dispatch_id)
        );
        CREATE TABLE mls_group_received (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            sender BLOB NOT NULL, dispatch_id BLOB NOT NULL,
            accepted_at_ms INTEGER NOT NULL, payload BLOB NOT NULL,
            applied INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(group_id,sender,dispatch_id)
        );
    "#,
    ),
    M::up(r#"
        CREATE TABLE mls_migration_consents (
            group_id BLOB NOT NULL, branch BLOB NOT NULL,
            who BLOB NOT NULL, signature BLOB NOT NULL,
            last_sent INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(group_id,who)
        );
        CREATE TABLE mls_group_migrations (
            group_id BLOB PRIMARY KEY, target BLOB NOT NULL UNIQUE,
            conversation BLOB NOT NULL
        );
    "#),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub fn migrate(conn: &mut Connection) {
    super::prepare(conn, &MIGRATIONS);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Messages buffered for a future epoch before dispatch identities were recorded survive the
    /// migration that added them, with no identity invented.
    #[test]
    fn dispatch_identity_migration_preserves_legacy_buffered_messages() {
        let mut conn = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATION_ARRAY[..7]).to_latest(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO mls_epoch_ahead \
             (group_id, epoch, dispatch_id, msg_blob, received_at_ms, accepted_at_ms) \
             VALUES (?1, 2, ?2, ?3, 456, 123)",
            (vec![1u8; 32], vec![2u8; 16], vec![3u8; 128]),
        )
        .unwrap();
        migrate(&mut conn);
        let row = conn
            .query_row(
                "SELECT dispatch_id, msg_blob, accepted_at_ms, original_dispatch_id, \
                 dispatch_sender FROM mls_epoch_ahead",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        let legacy: (Vec<u8>, Vec<u8>, u64, Option<Vec<u8>>, Option<Vec<u8>>) =
            (vec![2; 16], vec![3; 128], 123, None, None);
        assert_eq!(row, legacy);
    }
}
