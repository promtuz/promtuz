use std::{fs, path::Path, process};

mod macros;

pub mod identity;
pub mod messages;
pub mod mls;
pub mod network;
pub mod outbox;
pub mod peers;
pub mod utils;

static PACKAGE_NAME: &str = "com.promtuz.chat";

pub fn db(file_name: &'static str) -> String {
    // On-device this is the fixed Android package data dir. Off-device
    // (the e2e sandbox, or any host run) `PROMTUZ_DATA_DIR` redirects every
    // libcore store into an isolated temp dir, so N client processes don't
    // collide on the one global path. Unset → exact on-device behaviour.
    let db_dir = std::env::var("PROMTUZ_DATA_DIR")
        .unwrap_or_else(|_| format!("/data/data/{PACKAGE_NAME}/databases"));
    let dir_path = Path::new(&db_dir);

    // `create_dir_all` (was `create_dir`): an override dir may be nested
    // (e.g. `/tmp/promtuz-e2e/client-0/databases`) with missing parents.
    if !dir_path.is_dir() && fs::create_dir_all(dir_path).is_err() {
        log::error!("Failed to create database directory!");
        process::exit(1);
    }

    format!("{db_dir}/{file_name}.db")
}

/// Directory for large on-disk byte blobs (transfer partials, etc.) — the file
/// counterpart to [`db`]. Same base as `db` (`PROMTUZ_DATA_DIR` override, else
/// the on-device package dir) but a `files/<sub>` subtree, created on demand.
pub fn files_dir(sub: &str) -> String {
    let base = std::env::var("PROMTUZ_DATA_DIR")
        .unwrap_or_else(|_| format!("/data/data/{PACKAGE_NAME}"));
    let dir = format!("{base}/files/{sub}");
    if !Path::new(&dir).is_dir() && fs::create_dir_all(&dir).is_err() {
        log::error!("Failed to create files directory!");
        process::exit(1);
    }
    dir
}

/// Ring the client's `on_db_changed` doorbell whenever `conn` commits a write —
/// the reactive-UI trigger. `tables` is the coarse set this connection owns; the
/// UI re-reads any observed query overlapping them. Content-free (the DB is the
/// truth). No-op until the client installs its event sink, so startup migrations
/// don't fire it. The hook runs on the writing thread with the conn locked, so
/// the client impl must only wake a flow — never block or call back into core.
pub(crate) fn register_change_hook(conn: &rusqlite::Connection, tables: &[&str]) {
    let tables: Vec<String> = tables.iter().map(|s| (*s).to_string()).collect();
    conn.commit_hook(Some(move || {
        if let Some(ev) = crate::platform::EVENTS.get() {
            ev.on_db_changed(tables.clone());
        }
        false // observe only — never roll back the commit
    }));
}
