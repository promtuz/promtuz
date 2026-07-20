//! Local persistence for in-flight transfers: what the sender still holds
//! (`retention`) and what a receiver has partially pulled (`partials`), plus
//! the on-disk location of the partial bytes.

use log::info;
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
    info!("DB: TRANSFERS_DB CONNECTED");

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

/// Drop every entry that expired at or before `now`. The `>= 0` guard skips the
/// u64::MAX sentinel (stored as -1 by the bitwise cast) so it never expires.
pub fn retention_gc(now: u64) -> usize {
    TRANSFERS_DB
        .lock()
        .execute(
            "DELETE FROM retention WHERE expires_at >= 0 AND expires_at <= ?1",
            params![now as i64],
        )
        .expect("retention gc")
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

pub fn partial_have(file_id: &[u8; 32]) -> u32 {
    TRANSFERS_DB
        .lock()
        .query_row("SELECT have FROM partials WHERE file_id = ?1", params![file_id], |r| r.get(0))
        .optional()
        .expect("partial_have read")
        .unwrap_or(0)
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
        .prepare("SELECT path FROM partials WHERE state IN (?1, ?2) AND updated_at < ?3")
        .expect("gc_dead_partials prepare");
    let paths: Vec<String> = stmt
        .query_map(params![FAILED, HELD, older_than as i64], |r| r.get(0))
        .expect("gc_dead_partials query")
        .collect::<rusqlite::Result<_>>()
        .expect("gc_dead_partials rows");
    drop(stmt);
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    conn.execute(
        "DELETE FROM partials WHERE state IN (?1, ?2) AND updated_at < ?3",
        params![FAILED, HELD, older_than as i64],
    )
    .expect("gc_dead_partials delete");
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_and_partial_roundtrip() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let fid = [5u8; 32];
        retention_put(&fid, "/tmp/x", 100, 256 * 1024, &vec![1, 2, 3], u64::MAX).unwrap();
        assert!(retention_get(&fid).is_some());
        let p = Partial {
            file_id: fid,
            source_ipk: [6u8; 32],
            total: 100,
            chunk_size: 256 * 1024,
            manifest: None,
            have: 2,
            state: ACTIVE,
            path: "/tmp/p".into(),
            updated_at: 1,
        };
        partial_put(&p).unwrap();
        assert_eq!(partial_have(&fid), 2);
    }

    #[test]
    fn retention_gc_drops_expired_keeps_sentinel() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) };

        let never = [7u8; 32]; // u64::MAX sentinel — never garbage-collected
        let soon = [8u8; 32]; // expires at t=10
        retention_put(&never, "/tmp/n", 1, 1, &[], u64::MAX).unwrap();
        retention_put(&soon, "/tmp/s", 1, 1, &[], 10).unwrap();

        retention_gc(20); // now past soon's expiry
        assert!(retention_get(&never).is_some());
        assert!(retention_get(&soon).is_none());
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
}
