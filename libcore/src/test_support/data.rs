use ed25519_dalek::SigningKey;
use rusqlite::Connection;
use tempfile::TempDir;

use crate::db::Stores;

/// A fresh in-memory database with the real migrations, e.g. `open(db::messages::migrate)`.
pub(crate) fn open(migrate: fn(&mut Connection)) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn);
    conn
}

/// One device's databases, all in memory. The directory backs `files_dir` and lives as long as
/// the returned handle.
pub(crate) fn stores() -> (Stores, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Stores::in_memory(dir.path().display().to_string()), dir)
}

/// Saves the local identity derived from seed `n`, its key sealed as `ScopedCore` seals, and
/// returns that key.
pub(crate) fn identity(conn: &Connection, n: u8) -> SigningKey {
    let key = SigningKey::from_bytes(&[n; 32]);
    conn.execute(
        "INSERT INTO identity (id, ipk, enc_isk, created_at, name) VALUES (0, ?1, ?2, 0, 'me')",
        (key.verifying_key().as_bytes(), key.as_bytes()),
    )
    .unwrap();
    key
}

/// The `n`th ULID, so rows sort by `n` without waiting on the clock.
pub(crate) fn ulid(n: u8) -> String {
    ulid::Ulid::from_parts(u64::from(n), 0).to_string()
}

/// Runs `f` while every `op` (`INSERT`, `UPDATE` or `DELETE`) on `table` fails, as a full disk
/// would fail it.
pub(crate) fn with_failing_trigger<T>(
    conn: &mut Connection, table: &str, op: &str, f: impl FnOnce(&mut Connection) -> T,
) -> T {
    conn.execute_batch(&format!(
        "CREATE TEMP TRIGGER injected_failure BEFORE {op} ON {table} \
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
    ))
    .unwrap();
    let out = f(conn);
    conn.execute_batch("DROP TRIGGER temp.injected_failure;").unwrap();
    out
}
