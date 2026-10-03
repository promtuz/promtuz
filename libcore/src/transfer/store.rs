//! Transfer persistence: what a sender still serves (`retention`) and what a receiver has pulled
//! (`partials`).

use std::collections::HashMap;
use std::sync::Arc;

use std::sync::LazyLock;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};
use tokio_util::sync::CancellationToken;

use crate::db::Stores;
use crate::state::core;

/// Download states for a `partials` row.
pub const PENDING: u8 = 0;
pub const ACTIVE: u8 = 1;
pub const DONE: u8 = 2;
pub const FAILED: u8 = 3;
pub const HELD: u8 = 4;
/// From the tap until a link forms (then ACTIVE) or the attempt gives up (then HELD).
pub const CONNECTING: u8 = 5;

#[derive(Debug, Clone)]
pub struct Retention {
    pub path: String,
    pub size: u64,
    pub chunk_size: u32,
    pub manifest: Vec<u8>,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
pub struct Partial {
    pub file_id: [u8; 32],
    pub source_ipk: [u8; 32],
    pub total: u64,
    pub chunk_size: u32,
    pub manifest: Option<Vec<u8>>,
    pub have: u32,
    pub state: u8,
    pub path: String,
    pub updated_at: u64,
}

impl Partial {
    /// `DONE` can outlive its file (a chat clear unlinks it), so the path must exist too.
    pub fn is_complete(&self) -> bool {
        self.state == DONE && std::path::Path::new(&self.path).exists()
    }
}

const MIGRATION_ARRAY: &[M] = &[
    M::up(
        r#"--sql
        CREATE TABLE retention (
          file_id     BLOB PRIMARY KEY CHECK(length(file_id) = 32),
          path        TEXT NOT NULL,
          size        INTEGER NOT NULL,
          chunk_size  INTEGER NOT NULL,
          manifest    BLOB NOT NULL,
          expires_at  INTEGER NOT NULL   -- u64 stored bitwise; u64::MAX = never
        );
        CREATE TABLE partials (
          file_id     BLOB PRIMARY KEY CHECK(length(file_id) = 32),
          source_ipk  BLOB NOT NULL CHECK(length(source_ipk) = 32),
          total       INTEGER NOT NULL,
          chunk_size  INTEGER NOT NULL,
          manifest    BLOB,
          have        INTEGER NOT NULL DEFAULT 0,
          state       INTEGER NOT NULL DEFAULT 0,
          path        TEXT NOT NULL,
          updated_at  INTEGER NOT NULL
        );
    "#,
    ),
    M::up(
        "ALTER TABLE partials ADD COLUMN last_wake_at INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE partials ADD COLUMN retry_after INTEGER NOT NULL DEFAULT 0;",
    ),
    M::up("ALTER TABLE partials ADD COLUMN wake_pending INTEGER NOT NULL DEFAULT 0;"),
    // NULL marks an older prefix-only partial; its bytes are rechecked before any bitmap.
    M::up("ALTER TABLE partials ADD COLUMN verified BLOB;"),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub fn migrate(conn: &mut Connection) {
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    MIGRATIONS.to_latest(conn).expect("db migration failed");
}

pub fn partial_path(db: &Stores, file_id: &[u8; 32]) -> String {
    format!("{}/{}.part", db.files_dir("transfers"), hex::encode(file_id))
}

pub(crate) fn retention_put_tx(
    conn: &Connection, file_id: &[u8; 32], path: &str, size: u64, chunk_size: u32,
    manifest: &[u8], expires_at: u64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO retention
           (file_id, path, size, chunk_size, manifest, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![file_id, path, size, chunk_size, manifest, expires_at as i64],
    )?;
    Ok(())
}

pub fn retention_get(file_id: &[u8; 32]) -> Option<Retention> {
    retention_get_tx(&core().db.transfers().lock(), file_id)
}

pub(crate) fn retention_get_tx(conn: &Connection, file_id: &[u8; 32]) -> Option<Retention> {
    conn.query_row(
        "SELECT path, size, chunk_size, manifest, expires_at FROM retention WHERE file_id = ?1",
        params![file_id],
        |r| {
            Ok(Retention {
                path: r.get("path")?,
                size: r.get("size")?,
                chunk_size: r.get("chunk_size")?,
                manifest: r.get("manifest")?,
                expires_at: r.get::<_, i64>("expires_at")? as u64,
            })
        },
    )
    .optional()
    .expect("retention read")
}

pub(crate) fn retention_file_ids_tx(conn: &Connection) -> Vec<[u8; 32]> {
    let mut stmt = match conn.prepare("SELECT file_id FROM retention") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
        .map(|rows| rows.filter_map(|r| r.ok()?.try_into().ok()).collect())
        .unwrap_or_default()
}

pub fn partial_get(file_id: &[u8; 32]) -> Option<Partial> {
    partial_get_tx(&core().db.transfers().lock(), file_id)
}

pub(crate) fn partial_get_tx(conn: &Connection, file_id: &[u8; 32]) -> Option<Partial> {
    conn.query_row("SELECT * FROM partials WHERE file_id = ?1", params![file_id], |r| {
        Ok(Partial {
            file_id: r.get("file_id")?,
            source_ipk: r.get("source_ipk")?,
            total: r.get("total")?,
            chunk_size: r.get("chunk_size")?,
            manifest: r.get("manifest")?,
            have: r.get("have")?,
            state: r.get("state")?,
            path: r.get("path")?,
            updated_at: r.get("updated_at")?,
        })
    })
    .optional()
    .expect("partial read")
}

fn partial_put_locked(conn: &Connection, p: &Partial) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO partials
           (file_id, source_ipk, total, chunk_size, manifest, have, state, path, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(file_id) DO UPDATE SET source_ipk=excluded.source_ipk,
           total=excluded.total, chunk_size=excluded.chunk_size, manifest=excluded.manifest,
           have=excluded.have, state=excluded.state, path=excluded.path, updated_at=excluded.updated_at",
        params![
            p.file_id, p.source_ipk, p.total, p.chunk_size, p.manifest, p.have, p.state, p.path,
            p.updated_at
        ],
    )?;
    Ok(())
}

/// One live receiver per file. Deletion cancels it under the transfers lock, which also guards
/// opening files and publishing progress, so a late worker cannot recreate a deleted path or row.
static RECEIVERS: LazyLock<Mutex<HashMap<[u8; 32], Arc<CancellationToken>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub(crate) struct ReceiverLease {
    file_id: [u8; 32],
    pub(crate) cancel: Arc<CancellationToken>,
    // Deregisters only when the last owner, possibly a background recovery, has stopped.
    _owner: Arc<ReceiverOwner>,
}

struct ReceiverOwner {
    file_id: [u8; 32],
    cancel: Arc<CancellationToken>,
}

impl Drop for ReceiverOwner {
    fn drop(&mut self) {
        let mut receivers = RECEIVERS.lock();
        if receivers.get(&self.file_id).is_some_and(|token| Arc::ptr_eq(token, &self.cancel)) {
            receivers.remove(&self.file_id);
        }
    }
}

pub(crate) fn receiver_lease(db: &Stores, file_id: [u8; 32]) -> ReceiverLease {
    let _db = db.transfers().lock();
    let cancel = Arc::new(CancellationToken::new());
    if let Some(previous) = RECEIVERS.lock().insert(file_id, cancel.clone()) {
        previous.cancel();
    }
    let owner = Arc::new(ReceiverOwner { file_id, cancel: cancel.clone() });
    ReceiverLease { file_id, cancel, _owner: owner }
}

pub(crate) fn partial_put_live_tx(
    conn: &Connection, p: &Partial, lease: &ReceiverLease,
) -> anyhow::Result<()> {
    ensure_live(lease)?;
    partial_put_locked(conn, p)?;
    Ok(())
}

/// `None` for a prefix-only partial. A set bit is only a candidate until its bytes are rechecked.
pub(crate) fn verified_bitmap_tx(
    conn: &Connection, file_id: &[u8; 32],
) -> rusqlite::Result<Option<Vec<u8>>> {
    conn.query_row(
        // No valid bitmap exceeds 32 KiB; a larger corrupt blob reads as empty, unallocated.
        "SELECT CASE WHEN length(verified)>32768 THEN x'' ELSE verified END
         FROM partials WHERE file_id=?1",
        params![file_id],
        |r| r.get::<_, Option<Vec<u8>>>(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Publishes the bitmap and the prefix in one transaction, so neither claims bytes the other lacks.
pub(crate) fn partial_put_verified_live_tx(
    conn: &mut Connection, p: &Partial, verified: &[u8], lease: &ReceiverLease,
) -> anyhow::Result<()> {
    ensure_live(lease)?;
    anyhow::ensure!(p.file_id == lease.file_id, "receiver lease belongs to another file");
    partial_put_verified_locked(conn, p, verified)?;
    Ok(())
}

fn partial_put_verified_locked(
    db: &mut Connection, p: &Partial, verified: &[u8],
) -> rusqlite::Result<()> {
    let tx = db.transaction()?;
    partial_put_locked(&tx, p)?;
    tx.execute("UPDATE partials SET verified=?2 WHERE file_id=?1", params![p.file_id, verified])?;
    tx.commit()
}

/// An UPDATE, not an upsert, so it can never reinsert a row that deletion removed.
pub(crate) fn partial_progress_verified_live_tx(
    conn: &Connection, file_id: &[u8; 32], have: u32, state: u8, updated_at: u64,
    verified: &[u8], lease: &ReceiverLease,
) -> anyhow::Result<()> {
    ensure_live(lease)?;
    anyhow::ensure!(*file_id == lease.file_id, "receiver lease belongs to another file");
    let changed = conn.execute(
        "UPDATE partials SET have=?2, state=?3, updated_at=?4, verified=?5 WHERE file_id=?1",
        params![file_id, have, state, updated_at, verified],
    )?;
    if changed == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    Ok(())
}

/// UI progress counts verified chunks; `Partial::have` stays the contiguous prefix old peers need.
/// An invalid bitmap counts as nothing.
pub(crate) fn verified_count(p: &Partial) -> u32 {
    verified_count_tx(&core().db.transfers().lock(), p)
}

pub(crate) fn verified_count_tx(conn: &Connection, p: &Partial) -> u32 {
    let chunks = p.total.div_ceil(p.chunk_size.max(1) as u64);
    let prefix = (p.have as u64).min(chunks) as u32;
    let Ok(Some(bits)) = verified_bitmap_tx(conn, &p.file_id) else { return prefix };
    if chunks > (8 * 1024 * 1024 / 32)
        || bits.len() as u64 != chunks.div_ceil(8)
        || (chunks % 8 != 0 && bits.last().is_some_and(|last| last >> (chunks % 8) != 0))
    {
        return 0;
    }
    bits.iter().map(|byte| byte.count_ones()).sum()
}

pub(crate) fn open_partial(
    db: &Stores, lease: &ReceiverLease, path: &str,
) -> anyhow::Result<std::fs::File> {
    let _db = db.transfers().lock();
    ensure_live(lease)?;
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)?)
}

#[derive(Debug, thiserror::Error)]
#[error("attachment download cancelled")]
pub(crate) struct Cancelled;

fn ensure_live(lease: &ReceiverLease) -> anyhow::Result<()> {
    if lease.cancel.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}

/// Claimed before sending, even if the wake fails, so reconnects cannot fan out push traffic. The
/// limit is per sender: one reverse wake brings the link for every pending file.
pub(crate) fn claim_wake_tx(
    conn: &mut Connection, file_id: &[u8; 32], now: u64, backoff: u64,
) -> rusqlite::Result<bool> {
    let tx = conn.transaction()?;
    let claimed = tx.execute(
        "UPDATE partials SET last_wake_at = ?2 WHERE file_id = ?1 AND state = ?4
         AND NOT EXISTS (
           SELECT 1 FROM partials recent
            WHERE recent.source_ipk = partials.source_ipk
              AND recent.last_wake_at > 0 AND recent.last_wake_at > ?3
         )",
        params![file_id, now, now.saturating_sub(backoff), HELD],
    )? != 0;
    if claimed {
        // One dial-back serves every waiting file from this sender: one cooldown override in all.
        tx.execute(
            "UPDATE partials SET wake_pending=1 WHERE state=?2 AND source_ipk =
            (SELECT source_ipk FROM partials WHERE file_id=?1)",
            params![file_id, HELD],
        )?;
    }
    tx.commit()?;
    Ok(claimed)
}

pub(crate) fn claim_ready_retry_tx(conn: &Connection, file_id: &[u8; 32]) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "UPDATE partials SET wake_pending=0
        WHERE file_id=?1 AND state=?2 AND wake_pending=1",
        params![file_id, HELD],
    )? != 0)
}

pub(crate) fn retry_after_tx(conn: &Connection, file_id: &[u8; 32]) -> u64 {
    conn.query_row("SELECT retry_after FROM partials WHERE file_id = ?1", params![file_id], |r| {
        r.get(0)
    })
    .optional()
    .expect("retry read")
    .unwrap_or(0)
}

pub(crate) fn defer_retry_tx(conn: &Connection, file_id: &[u8; 32], until: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE partials SET retry_after = ?2 WHERE file_id = ?1",
        params![file_id, until],
    )?;
    Ok(())
}

/// Never selects `DONE`: that `.part` is the delivered attachment the user keeps.
pub(crate) fn gc_dead_partials_tx(conn: &Connection, older_than: u64) -> Vec<String> {
    let mut stmt = conn
        .prepare(
            "SELECT file_id, path FROM partials WHERE state IN (?1, ?2, ?3) AND updated_at < ?4",
        )
        .expect("gc_dead_partials prepare");
    let rows: Vec<([u8; 32], String)> = stmt
        .query_map(params![FAILED, HELD, CONNECTING, older_than as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("gc_dead_partials query")
        .collect::<rusqlite::Result<_>>()
        .expect("gc_dead_partials rows");
    drop(stmt);
    // A live receiver owns its partial even while waiting to retry.
    let active = RECEIVERS.lock();
    let paths: Vec<String> = rows
        .into_iter()
        .filter(|(fid, _)| !active.contains_key(fid))
        .map(|(_, path)| path)
        .collect();
    drop(active);
    for p in &paths {
        let _ = std::fs::remove_file(p);
        conn.execute("DELETE FROM partials WHERE path = ?1", params![p])
            .expect("gc_dead_partials delete");
    }
    paths
}

/// Removes every row naming `file_id` and its bytes, once no message points at it. Deleted means
/// gone: a peer that had not pulled it yet gets `Gone`. Best effort.
pub fn forget_file(db: &Stores, file_id: &[u8; 32]) {
    let conn = db.transfers().lock();
    forget_row_tx(&conn, "partials", file_id, Some(partial_path(db, file_id)));
    forget_row_tx(&conn, "retention", file_id, None);
}

pub(crate) fn forget_row_tx(
    conn: &Connection, table: &str, file_id: &[u8; 32], fallback: Option<String>,
) {
    if table == "partials"
        && let Some(cancel) = RECEIVERS.lock().get(file_id)
    {
        cancel.cancel();
    }
    // The row's `path` is the `local_path` handed out; without it, try the canonical `.part`.
    let sql = format!("SELECT path FROM {table} WHERE file_id = ?1");
    let path = match conn.query_row(&sql, params![file_id], |r| r.get::<_, String>(0)) {
        Ok(p) => Some(p),
        Err(rusqlite::Error::QueryReturnedNoRows) => fallback,
        Err(e) => {
            log::warn!("transfer: {table} path read failed: {e}");
            fallback
        },
    };
    // The row goes first: a row over deleted bytes is worse than leaked disk.
    if let Err(e) =
        conn.execute(&format!("DELETE FROM {table} WHERE file_id = ?1"), params![file_id])
    {
        log::warn!("transfer: {table} row for a forgotten file survives: {e}");
    }
    if let Some(path) = path
        && let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("transfer: {path} left on disk: {e}");
    }
}

/// Resumable partials; the `DOWNLOADING` guard makes re-driving a live pull a no-op.
pub(crate) fn incomplete_file_ids_tx(conn: &Connection) -> Vec<[u8; 32]> {
    let mut stmt = conn
        .prepare("SELECT file_id FROM partials WHERE state IN (?1, ?2, ?3)")
        .expect("incomplete_file_ids prepare");
    stmt.query_map(params![HELD, ACTIVE, CONNECTING], |r| r.get(0))
        .expect("incomplete_file_ids query")
        .collect::<rusqlite::Result<_>>()
        .expect("incomplete_file_ids rows")
}

pub(crate) fn incomplete_file_ids_for_tx(conn: &Connection, peer: &[u8; 32]) -> Vec<[u8; 32]> {
    let mut stmt = conn
        .prepare("SELECT file_id FROM partials WHERE source_ipk = ?1 AND state IN (?2, ?3, ?4)")
        .expect("incomplete_file_ids_for prepare");
    stmt.query_map(params![peer.as_slice(), HELD, ACTIVE, CONNECTING], |r| r.get(0))
        .expect("incomplete_file_ids_for query")
        .collect::<rusqlite::Result<_>>()
        .expect("incomplete_file_ids_for rows")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn row(file_id: [u8; 32], state: u8, path: &str, updated_at: u64) -> Partial {
        Partial {
            file_id,
            source_ipk: [9; 32],
            total: 5,
            chunk_size: 5,
            manifest: None,
            have: 1,
            state,
            path: path.into(),
            updated_at,
        }
    }

    #[test]
    fn migration_preserves_prefix_and_updates_preserve_retry_history() {
        let mut db = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATION_ARRAY[..1]).to_latest(&mut db).unwrap();
        let mut p = row([0xc1; 32], HELD, "/not-opened", 100);
        partial_put_locked(&db, &p).unwrap();
        MIGRATIONS.to_latest(&mut db).unwrap();
        let history = "SELECT have, last_wake_at, retry_after, verified FROM partials";
        let read = |db: &Connection| {
            db.query_row(history, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap()
        };
        let migrated: (u32, u64, u64, Option<Vec<u8>>) = read(&db);
        assert_eq!(migrated, (1, 0, 0, None));
        db.execute(
            "UPDATE partials SET last_wake_at = 110, retry_after = 170, verified = x'01'",
            [],
        )
        .unwrap();
        p.state = CONNECTING;
        p.updated_at = 120;
        partial_put_locked(&db, &p).unwrap();
        let kept: (u32, u64, u64, Option<Vec<u8>>) = read(&db);
        assert_eq!(
            kept,
            (1, 110, 170, Some(vec![1])),
            "a state change keeps retry and range history"
        );
    }

    #[test]
    fn cleanup_takes_dead_rows_with_their_bytes_but_never_a_delivered_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::test_support::data::open(migrate);
        let file = |name: &str| {
            let path = dir.path().join(name).display().to_string();
            std::fs::write(&path, b"bytes").unwrap();
            path
        };
        let (dead, done, fresh, sent) = (file("dead"), file("done"), file("fresh"), file("sent"));
        partial_put_locked(&db, &row([0xd1; 32], FAILED, &dead, 100)).unwrap();
        partial_put_locked(&db, &row([0xd2; 32], DONE, &done, 100)).unwrap();
        partial_put_locked(&db, &row([0xd3; 32], FAILED, &fresh, 5000)).unwrap();
        assert_eq!(gc_dead_partials_tx(&db, 1000), vec![dead.clone()]);
        assert!(partial_get_tx(&db, &[0xd1; 32]).is_none() && !Path::new(&dead).exists());
        assert!(partial_get_tx(&db, &[0xd2; 32]).unwrap().is_complete(), "a delivered file stays");
        assert!(partial_get_tx(&db, &[0xd3; 32]).is_some() && Path::new(&fresh).exists());

        retention_put_tx(&db, &[0xd2; 32], &sent, 5, 5, &[], u64::MAX).unwrap();
        forget_row_tx(&db, "partials", &[0xd2; 32], None);
        forget_row_tx(&db, "retention", &[0xd2; 32], None);
        assert!(partial_get_tx(&db, &[0xd2; 32]).is_none() && !Path::new(&done).exists());
        assert!(retention_get_tx(&db, &[0xd2; 32]).is_none() && !Path::new(&sent).exists());
        assert!(
            !row([0xd2; 32], DONE, &done, 0).is_complete(),
            "DONE without its bytes is not done"
        );
    }
}
