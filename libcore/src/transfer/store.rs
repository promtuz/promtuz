//! Local persistence for in-flight transfers: what the sender still holds
//! (`retention`) and what a receiver has partially pulled (`partials`), plus
//! the on-disk location of the partial bytes.

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use rusqlite_migration::{M, Migrations};

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

const MIGRATION_ARRAY: &[M] = &[M::up(
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
)];
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
    file_id: &[u8; 32],
    path: &str,
    size: u64,
    chunk_size: u32,
    manifest: &[u8],
    expires_at: u64,
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
    TRANSFERS_DB.lock().execute(
        "INSERT OR REPLACE INTO partials
           (file_id, source_ipk, total, chunk_size, manifest, have, state, path, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            p.file_id, p.source_ipk, p.total, p.chunk_size, p.manifest, p.have, p.state, p.path,
            p.updated_at
        ],
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
        .prepare("SELECT path FROM partials WHERE state IN (?1, ?2, ?3) AND updated_at < ?4")
        .expect("gc_dead_partials prepare");
    let paths: Vec<String> = stmt
        .query_map(params![FAILED, HELD, CONNECTING, older_than as i64], |r| r.get(0))
        .expect("gc_dead_partials query")
        .collect::<rusqlite::Result<_>>()
        .expect("gc_dead_partials rows");
    drop(stmt);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    conn.execute(
        "DELETE FROM partials WHERE state IN (?1, ?2, ?3) AND updated_at < ?4",
        params![FAILED, HELD, CONNECTING, older_than as i64],
    )
    .expect("gc_dead_partials delete");
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
    if let Err(e) = conn.execute(&format!("DELETE FROM {table} WHERE file_id = ?1"), params![file_id]) {
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
