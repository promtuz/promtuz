//! Local persistence for in-flight transfers: what the sender still holds
//! (`retention`) and what a receiver has partially pulled (`partials`), plus
//! the on-disk location of the partial bytes.

use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};
use tokio_util::sync::CancellationToken;

/// Download states for a `partials` row.
pub const PENDING: u8 = 0;
pub const ACTIVE: u8 = 1;
pub const DONE: u8 = 2;
pub const FAILED: u8 = 3;
pub const HELD: u8 = 4;
/// Reaching the sender: from the tap until a link forms (then ACTIVE) or
/// the attempt gives up (then HELD). What the card shows meanwhile.
pub const CONNECTING: u8 = 5;

/// Sender-side: the manifest + source bytes we keep serving until `expires_at`.
#[derive(Debug, Clone)]
pub struct Retention {
    pub path: String,
    pub size: u64,
    pub chunk_size: u32,
    pub manifest: Vec<u8>,
    pub expires_at: u64,
}

/// Receiver-side: how far a pull has progressed for one `file_id`.
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
    /// `DONE` *and* the bytes are still there — the question both readers of a
    /// finished transfer actually ask.
    ///
    /// The state alone can't answer it. Clearing a chat unlinks the `.part`
    /// while a live pull holds the fd, and that pull's next progress write puts
    /// the row straight back, leaving `DONE` over a path that is gone. A
    /// `file_id` is a content hash, so every later message carrying the same
    /// content resolves to that row; nothing re-pulls a file the row calls
    /// finished and [`gc_dead_partials`] spares `DONE`, so the hash would stay
    /// poisoned for good. One `stat` per read settles it.
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
    // NULL identifies an older prefix-only partial. The range receiver checks
    // those bytes against the manifest before publishing a verified bitmap.
    M::up("ALTER TABLE partials ADD COLUMN verified BLOB;"),
];
const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

pub static TRANSFERS_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let mut conn = Connection::open(crate::db::db("transfers")).expect("db open failed");
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    MIGRATIONS.to_latest(&mut conn).expect("db migration failed");
    // Partials advance as chunks land; the doorbell lets the UI re-read progress.
    crate::db::register_change_hook(&conn, &["partials"]);

    Mutex::new(conn)
});

/// On-disk location of a receiver's partial bytes for `file_id`.
pub fn partial_path(file_id: &[u8; 32]) -> String {
    format!("{}/{}.part", crate::db::files_dir("transfers"), hex::encode(file_id))
}

pub fn retention_put(
    file_id: &[u8; 32], path: &str, size: u64, chunk_size: u32, manifest: &[u8], expires_at: u64,
) -> rusqlite::Result<()> {
    TRANSFERS_DB.lock().execute(
        "INSERT OR REPLACE INTO retention
           (file_id, path, size, chunk_size, manifest, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![file_id, path, size, chunk_size, manifest, expires_at as i64],
    )?;
    Ok(())
}

pub fn retention_get(file_id: &[u8; 32]) -> Option<Retention> {
    TRANSFERS_DB
        .lock()
        .query_row(
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

/// Every retained `file_id`, for the startup sweep.
pub fn retention_file_ids() -> Vec<[u8; 32]> {
    let conn = TRANSFERS_DB.lock();
    let mut stmt = match conn.prepare("SELECT file_id FROM retention") {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
        .map(|rows| rows.filter_map(|r| r.ok()?.try_into().ok()).collect())
        .unwrap_or_default()
}

pub fn partial_get(file_id: &[u8; 32]) -> Option<Partial> {
    TRANSFERS_DB
        .lock()
        .query_row("SELECT * FROM partials WHERE file_id = ?1", params![file_id], |r| {
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

pub fn partial_put(p: &Partial) -> rusqlite::Result<()> {
    partial_put_locked(&TRANSFERS_DB.lock(), p)
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

/// One live receiver generation. Deletion cancels it under TRANSFERS_DB, the
/// same lock used to open files and publish progress. A late worker can neither
/// recreate an unlinked path nor reinsert its row, even if the same file is
/// offered again before that worker observes cancellation.
static RECEIVERS: Lazy<Mutex<HashMap<[u8; 32], Arc<CancellationToken>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub(crate) struct ReceiverLease {
    file_id: [u8; 32],
    pub(crate) cancel: Arc<CancellationToken>,
    // Background recovery keeps this guard alive after its async caller is
    // dropped. Deregister only when the last owner has actually stopped.
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

pub(crate) fn receiver_lease(file_id: [u8; 32]) -> ReceiverLease {
    let _db = TRANSFERS_DB.lock();
    let cancel = Arc::new(CancellationToken::new());
    if let Some(previous) = RECEIVERS.lock().insert(file_id, cancel.clone()) {
        previous.cancel();
    }
    let owner = Arc::new(ReceiverOwner { file_id, cancel: cancel.clone() });
    ReceiverLease { file_id, cancel, _owner: owner }
}

#[cfg(test)]
pub(super) fn receiver_registered(file_id: &[u8; 32]) -> bool {
    RECEIVERS.lock().contains_key(file_id)
}

pub(crate) fn partial_put_live(p: &Partial, lease: &ReceiverLease) -> anyhow::Result<()> {
    let db = TRANSFERS_DB.lock();
    ensure_live(lease)?;
    partial_put_locked(&db, p)?;
    Ok(())
}

/// Stored chunk bitmap, or NULL for a partial created by the prefix protocol.
/// A set bit is only a recovery candidate until the receiver rechecks the
/// corresponding bytes. In-memory workers publish only after syncing data.
pub(crate) fn verified_bitmap(file_id: &[u8; 32]) -> rusqlite::Result<Option<Vec<u8>>> {
    TRANSFERS_DB
        .lock()
        .query_row(
            // An 8 MiB manifest cannot name more than 262144 hashes, so no
            // valid bitmap exceeds 32 KiB. Return an empty candidate set for
            // an oversized corrupt blob without allocating its full length.
            "SELECT CASE WHEN length(verified)>32768 THEN x'' ELSE verified END
             FROM partials WHERE file_id=?1",
            params![file_id],
            |r| r.get::<_, Option<Vec<u8>>>(0),
        )
        .optional()
        .map(Option::flatten)
}

/// Publish range progress and the contiguous legacy prefix together. A failed
/// bitmap write must not leave a prefix/state claiming bytes from that write.
pub(crate) fn partial_put_verified_live(
    p: &Partial, verified: &[u8], lease: &ReceiverLease,
) -> anyhow::Result<()> {
    let mut db = TRANSFERS_DB.lock();
    ensure_live(lease)?;
    anyhow::ensure!(p.file_id == lease.file_id, "receiver lease belongs to another file");
    partial_put_verified_locked(&mut db, p, verified)?;
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

/// Advance a live receiver without rewriting its immutable manifest on every
/// chunk. One UPDATE atomically publishes both progress representations; this
/// path cannot insert a row removed by deletion.
pub(crate) fn partial_progress_verified_live(
    file_id: &[u8; 32], have: u32, state: u8, updated_at: u64, verified: &[u8],
    lease: &ReceiverLease,
) -> anyhow::Result<()> {
    let db = TRANSFERS_DB.lock();
    ensure_live(lease)?;
    anyhow::ensure!(*file_id == lease.file_id, "receiver lease belongs to another file");
    let changed = db.execute(
        "UPDATE partials SET have=?2, state=?3, updated_at=?4, verified=?5 WHERE file_id=?1",
        params![file_id, have, state, updated_at, verified],
    )?;
    if changed == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows.into());
    }
    Ok(())
}

/// UI progress counts verified chunks, while `Partial::have` deliberately
/// remains the contiguous prefix required by old peers. Invalid local range
/// metadata is handled by the receiver; it must not inflate UI progress.
pub(crate) fn verified_count(p: &Partial) -> u32 {
    let chunks = p.total.div_ceil(p.chunk_size.max(1) as u64);
    let prefix = (p.have as u64).min(chunks) as u32;
    let Ok(Some(bits)) = verified_bitmap(&p.file_id) else { return prefix };
    if chunks > (8 * 1024 * 1024 / 32)
        || bits.len() as u64 != chunks.div_ceil(8)
        || (chunks % 8 != 0 && bits.last().is_some_and(|last| last >> (chunks % 8) != 0))
    {
        return 0;
    }
    bits.iter().map(|byte| byte.count_ones()).sum()
}

pub(crate) fn open_partial(lease: &ReceiverLease, path: &str) -> anyhow::Result<std::fs::File> {
    let _db = TRANSFERS_DB.lock();
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

/// Claim before sending, even when a wake fails: reconnects must not fan out
/// repeated push traffic while the sender stays offline. State/progress updates
/// deliberately leave this timestamp alone. Rate-limit by sender across all
/// their pending files: one reverse-wake establishes the link for every offer.
pub(crate) fn claim_wake(file_id: &[u8; 32], now: u64, backoff: u64) -> rusqlite::Result<bool> {
    let mut db = TRANSFERS_DB.lock();
    let tx = db.transaction()?;
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
        // A single dial-back can serve every waiting file from this sender.
        // Grant one cooldown override, not one new budget on every ready link.
        tx.execute(
            "UPDATE partials SET wake_pending=1 WHERE state=?2 AND source_ipk =
            (SELECT source_ipk FROM partials WHERE file_id=?1)",
            params![file_id, HELD],
        )?;
    }
    tx.commit()?;
    Ok(claimed)
}

pub(crate) fn claim_ready_retry(file_id: &[u8; 32]) -> rusqlite::Result<bool> {
    Ok(TRANSFERS_DB.lock().execute(
        "UPDATE partials SET wake_pending=0
        WHERE file_id=?1 AND state=?2 AND wake_pending=1",
        params![file_id, HELD],
    )? != 0)
}

pub(crate) fn retry_after(file_id: &[u8; 32]) -> u64 {
    TRANSFERS_DB
        .lock()
        .query_row("SELECT retry_after FROM partials WHERE file_id = ?1", params![file_id], |r| {
            r.get(0)
        })
        .optional()
        .expect("retry read")
        .unwrap_or(0)
}

pub(crate) fn defer_retry(file_id: &[u8; 32], until: u64) -> rusqlite::Result<()> {
    TRANSFERS_DB.lock().execute(
        "UPDATE partials SET retry_after = ?2 WHERE file_id = ?1",
        params![file_id, until],
    )?;
    Ok(())
}

/// Reap genuinely-abandoned receiver transfers: `FAILED`/`HELD` partials last
/// touched before `older_than`. A `DONE` partial is NEVER selected — its
/// `.part` file IS the delivered attachment the user keeps (`get_media`'s
/// `local_path`), so only junk bytes get unlinked. Unlink is best-effort (a
/// HELD row may have no file yet); the row is deleted regardless. Returns the
/// paths it removed.
pub fn gc_dead_partials(older_than: u64) -> Vec<String> {
    let conn = TRANSFERS_DB.lock();
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
    // A live receiver owns its partial even while waiting to retry. Do not
    // reap it under an active worker; deletion uses explicit cancellation.
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

/// Forget a file outright — every row that names it and the bytes under them —
/// for a `file_id` no message points at any more.
///
/// Both sides of a transfer live here. A receiver's `partials` row owns the
/// `.part` it pulled; a sender's `retention` row owns the copy the platform
/// handed [`crate::api::media::send_attachment`], which is core's to unlink
/// once nothing shows it. The state-based [`gc_dead_partials`] can't do this:
/// the case that matters is a `DONE` partial, which it spares by design. Row
/// and bytes go together because a `file_id` is a content hash: a surviving
/// `DONE` row would answer the same content arriving in some later message
/// with a `local_path` to a file nobody kept.
///
/// Deleted means gone, for the peer too: a recipient who had not pulled the
/// file yet is answered `Gone` from here on. The alternative — bytes that
/// outlive the message they were deleted with — is the wrong default for
/// this app; the sender chose to delete, the peer just chose to wait.
///
/// Best-effort throughout — a caller clearing a chat is not failed over a file
/// that won't unlink, and a `.part` that was never downloaded is simply absent.
pub fn forget_file(file_id: &[u8; 32]) {
    forget_partial(file_id);
    forget_retention(file_id);
}

/// The receiver half of [`forget_file`].
pub fn forget_partial(file_id: &[u8; 32]) {
    forget_row("partials", file_id, Some(partial_path(file_id)));
}

/// The sender half of [`forget_file`].
pub fn forget_retention(file_id: &[u8; 32]) {
    forget_row("retention", file_id, None);
}

fn forget_row(table: &str, file_id: &[u8; 32], fallback: Option<String>) {
    let conn = TRANSFERS_DB.lock();
    if table == "partials"
        && let Some(cancel) = RECEIVERS.lock().get(file_id)
    {
        cancel.cancel();
    }
    // The row's own `path` is what `get_media` hands out as `local_path`, so
    // that is the file to remove. Only "no such row" is a plain miss; a read
    // that failed says so. Either way a `.part`'s canonical location is where
    // one is written, so it stays the file to try.
    let sql = format!("SELECT path FROM {table} WHERE file_id = ?1");
    let path = match conn.query_row(&sql, params![file_id], |r| r.get::<_, String>(0)) {
        Ok(p) => Some(p),
        Err(rusqlite::Error::QueryReturnedNoRows) => fallback,
        Err(e) => {
            log::warn!("transfer: {table} path read failed: {e}");
            fallback
        },
    };
    // The row goes first. A DELETE that failed after the unlink would leave a
    // row standing over bytes that are gone; a row dropped while the file
    // survives only leaks disk, which the doc already accepts.
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

/// Every `file_id` whose partial is resumable — HELD (sender was offline) or
/// ACTIVE (a pull the process died mid-way, so nothing drives it now). The
/// reconnect retry re-drives each; the in-memory DOWNLOADING guard skips any a
/// live pull already owns, so re-driving a genuinely-active one is a no-op.
/// FAILED/DONE are excluded.
pub fn incomplete_file_ids() -> Vec<[u8; 32]> {
    let conn = TRANSFERS_DB.lock();
    let mut stmt = conn
        .prepare("SELECT file_id FROM partials WHERE state IN (?1, ?2, ?3)")
        .expect("incomplete_file_ids prepare");
    stmt.query_map(params![HELD, ACTIVE, CONNECTING], |r| r.get(0))
        .expect("incomplete_file_ids query")
        .collect::<rusqlite::Result<_>>()
        .expect("incomplete_file_ids rows")
}

/// [`incomplete_file_ids`] narrowed to pulls from one sender, for the
/// moment that sender becomes reachable.
pub fn incomplete_file_ids_for(peer: &[u8; 32]) -> Vec<[u8; 32]> {
    let conn = TRANSFERS_DB.lock();
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
    use super::*;

    #[test]
    fn migration_preserves_prefix_and_updates_preserve_retry_history() {
        let mut db = Connection::open_in_memory().unwrap();
        Migrations::from_slice(&MIGRATION_ARRAY[..1]).to_latest(&mut db).unwrap();
        let mut p = Partial {
            file_id: [0xc1; 32],
            source_ipk: [0xc2; 32],
            total: 100,
            chunk_size: 50,
            manifest: Some(vec![1]),
            have: 1,
            state: HELD,
            path: "/not-opened".into(),
            updated_at: 100,
        };
        partial_put_locked(&db, &p).unwrap();
        MIGRATIONS.to_latest(&mut db).unwrap();
        let initial: (u32, u64, u64, Option<Vec<u8>>) = db
            .query_row("SELECT have, last_wake_at, retry_after, verified FROM partials", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap();
        assert_eq!(initial, (1, 0, 0, None));
        db.execute("UPDATE partials SET last_wake_at=110, retry_after=170, verified=x'01'", [])
            .unwrap();
        p.state = CONNECTING;
        p.updated_at = 120;
        partial_put_locked(&db, &p).unwrap();
        let history: (u64, u64, Vec<u8>) = db
            .query_row("SELECT last_wake_at, retry_after, verified FROM partials", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(history, (110, 170, vec![1]), "UI transitions retain retry and range history");
    }

    #[test]
    fn deletion_cancels_old_generation_without_poisoning_new_offer() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let fid = [0xc3; 32];
        let p = Partial {
            file_id: fid,
            source_ipk: [0xc4; 32],
            total: 100,
            chunk_size: 50,
            manifest: None,
            have: 1,
            state: ACTIVE,
            path: partial_path(&fid),
            updated_at: 9000,
        };
        let old = receiver_lease(fid);
        partial_put_live(&p, &old).unwrap();
        let _open = open_partial(&old, &p.path).unwrap();
        forget_partial(&fid);
        assert!(old.cancel.is_cancelled());
        assert!(partial_put_live(&p, &old).is_err());
        assert!(open_partial(&old, &p.path).is_err());
        assert!(!std::path::Path::new(&p.path).exists());
        assert!(partial_get(&fid).is_none());
        let new = receiver_lease(fid);
        partial_put_live(&p, &new).unwrap();
        drop(old);
        forget_partial(&fid);
        assert!(new.cancel.is_cancelled(), "old lease drop must not detach the new lease");
        assert!(partial_put_live(&p, &new).is_err());
    }

    #[test]
    fn wake_claim_survives_connecting_and_is_atomic() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let fid = [0xc5; 32];
        let lease = receiver_lease(fid);
        let peer = [0xc6; 32];
        super::super::set_state(&fid, peer, HELD, &lease).unwrap();
        // A previous run may have left a timestamp; reset this fixture only.
        TRANSFERS_DB
            .lock()
            .execute("UPDATE partials SET last_wake_at=0 WHERE file_id=?1", params![fid])
            .unwrap();
        assert!(claim_wake(&fid, 100, 60).unwrap());
        super::super::set_state(&fid, peer, CONNECTING, &lease).unwrap();
        assert!(!claim_wake(&fid, 101, 60).unwrap());
        assert!(!claim_wake(&fid, 159, 60).unwrap());
        super::super::set_state(&fid, peer, HELD, &lease).unwrap();
        assert!(claim_wake(&fid, 160, 60).unwrap());
        assert!(!claim_wake(&fid, 160, 60).unwrap());
        let sibling = [0xc7; 32];
        let sibling_lease = receiver_lease(sibling);
        super::super::set_state(&sibling, peer, HELD, &sibling_lease).unwrap();
        assert!(
            !claim_wake(&sibling, 161, 60).unwrap(),
            "one wake covers this sender's other files"
        );
        forget_partial(&fid);
        forget_partial(&sibling);
    }

    #[test]
    fn ready_link_cannot_reset_exhausted_files_indefinitely() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };
        let peer = [0xca; 32];
        let a = [0xcb; 32];
        let b = [0xcc; 32];
        forget_partial(&a);
        forget_partial(&b);
        let lease_a = receiver_lease(a);
        let lease_b = receiver_lease(b);
        for (fid, lease) in [(a, &lease_a), (b, &lease_b)] {
            super::super::set_state(&fid, peer, HELD, lease).unwrap();
            defer_retry(&fid, 160).unwrap();
        }
        assert!(!claim_ready_retry(&a).unwrap());
        assert!(claim_wake(&a, 100, 60).unwrap());
        assert!(claim_ready_retry(&a).unwrap());
        assert!(claim_ready_retry(&b).unwrap(), "one wake enables other held files too");
        for _ in 0..5 {
            assert!(!claim_ready_retry(&a).unwrap());
            assert!(!claim_ready_retry(&b).unwrap());
            assert!(!claim_wake(&b, 102, 60).unwrap());
        }
        // Even after the cooldown expires, a sibling's internally-created
        // ready link cannot replenish either file's exhausted attempt budget.
        defer_retry(&a, 0).unwrap();
        defer_retry(&b, 0).unwrap();
        assert!(!claim_ready_retry(&a).unwrap());
        assert!(!claim_ready_retry(&b).unwrap());
        super::super::set_state(&a, peer, ACTIVE, &lease_a).unwrap();
        assert!(
            !claim_wake(&a, 200, 60).unwrap(),
            "an older held worker cannot wake an active successor"
        );
        forget_partial(&a);
        forget_partial(&b);
    }

    #[test]
    fn incomplete_file_ids_lists_held_and_active() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let mk = |fid: [u8; 32], state: u8| {
            partial_put(&Partial {
                file_id: fid,
                source_ipk: [1u8; 32],
                total: 1,
                chunk_size: 1,
                manifest: None,
                have: 0,
                state,
                path: partial_path(&fid),
                // Past any cutoff a sibling test sweeps with: `gc_dead_partials`
                // reaps FAILED/HELD across the whole process-global DB, and this
                // test is about which *states* resume, not about age.
                updated_at: 9_000,
            })
            .unwrap();
        };
        mk([0xe1; 32], HELD);
        mk([0xe2; 32], DONE);
        mk([0xe3; 32], FAILED);
        mk([0xe4; 32], ACTIVE);
        mk([0xe5; 32], HELD);

        let ids = incomplete_file_ids();
        assert!(ids.contains(&[0xe1; 32]) && ids.contains(&[0xe5; 32]), "HELD resumed");
        assert!(ids.contains(&[0xe4; 32]), "ACTIVE resumed");
        assert!(!ids.contains(&[0xe2; 32]), "DONE not resumed");
        assert!(!ids.contains(&[0xe3; 32]), "FAILED not resumed");
    }

    #[test]
    fn gc_dead_partials_reaps_dead_but_spares_done() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        // Cutoff t=1000: an old FAILED (reap), a DONE at the same age (KEEP —
        // its .part IS the delivered file), a fresh FAILED past the cutoff (KEEP).
        let mk = |fid: [u8; 32], state: u8, updated_at: u64| {
            let path = format!("{}/gc-{}.part", dir.display(), hex::encode(&fid[..2]));
            std::fs::write(&path, b"bytes").unwrap();
            partial_put(&Partial {
                file_id: fid,
                source_ipk: [9u8; 32],
                total: 5,
                chunk_size: 5,
                manifest: None,
                have: 0,
                state,
                path: path.clone(),
                updated_at,
            })
            .unwrap();
            path
        };
        let dead = mk([0xd1; 32], FAILED, 100);
        let done = mk([0xd2; 32], DONE, 100);
        let fresh = mk([0xd3; 32], FAILED, 5000);

        let removed = gc_dead_partials(1000);

        assert!(removed.contains(&dead));
        assert!(partial_get(&[0xd1; 32]).is_none(), "old FAILED row reaped");
        assert!(!std::path::Path::new(&dead).exists(), "old FAILED .part unlinked");

        assert!(partial_get(&[0xd2; 32]).is_some(), "DONE row spared");
        assert!(std::path::Path::new(&done).exists(), "DONE .part kept");

        assert!(partial_get(&[0xd3; 32]).is_some(), "fresh FAILED row spared");
        assert!(std::path::Path::new(&fresh).exists(), "fresh FAILED .part kept");
    }

    /// A file_id is a content hash, so a row outliving its bytes would tell a
    /// later message carrying the same content that the file is DONE and on
    /// disk. Rows and bytes leave together or not at all — the sender's
    /// retained copy included, since it is the only copy of what they sent.
    #[test]
    fn forget_file_takes_the_rows_and_the_bytes() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let fid = [0xf4; 32];
        let path = format!("{}/forget-f4.part", dir.display());
        std::fs::write(&path, b"bytes").unwrap();
        let sent = format!("{}/forget-f4.sent", dir.display());
        std::fs::write(&sent, b"bytes").unwrap();
        retention_put(&fid, &sent, 5, 5, &[], u64::MAX).unwrap();
        partial_put(&Partial {
            file_id: fid,
            source_ipk: [9u8; 32],
            total: 5,
            chunk_size: 5,
            manifest: None,
            have: 1,
            // DONE is exactly the state gc_dead_partials refuses to touch.
            state: DONE,
            path: path.clone(),
            updated_at: 9_000,
        })
        .unwrap();

        forget_file(&fid);

        assert!(partial_get(&fid).is_none(), "the partial row is gone");
        assert!(!std::path::Path::new(&path).exists(), "and so are its bytes");
        assert!(retention_get(&fid).is_none(), "the retention row is gone");
        assert!(!std::path::Path::new(&sent).exists(), "and so is the sent copy");
    }

    /// Nothing re-pulls a file the row calls finished, so a `DONE` row that
    /// outlived its `.part` would poison that content hash for good. Reading
    /// the state without the disk is what makes that possible.
    #[test]
    fn a_done_row_without_its_bytes_is_not_complete() {
        let path = std::env::temp_dir().join("promtuz-ghost.part").display().to_string();
        let _ = std::fs::remove_file(&path);
        let mut p = Partial {
            file_id: [0xf2; 32],
            source_ipk: [9u8; 32],
            total: 5,
            chunk_size: 5,
            manifest: None,
            have: 1,
            state: DONE,
            path: path.clone(),
            updated_at: 9_000,
        };

        assert!(!p.is_complete(), "DONE over a file that isn't there is not done");
        std::fs::write(&path, b"bytes").unwrap();
        assert!(p.is_complete(), "DONE with its bytes is");
        p.state = ACTIVE;
        assert!(!p.is_complete(), "and a pull still running never is");
        std::fs::remove_file(&path).unwrap();
    }
}
