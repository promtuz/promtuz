use std::fs;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::OnceLock;

use parking_lot::Mutex;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::Params;
use rusqlite::Row;
use rusqlite_migration::Migrations;

mod macros;
pub(crate) use macros::from_row;

pub mod identity;
pub mod messages;
pub mod mls;
pub mod network;
pub mod outbox;
pub mod peers;
pub mod utils;

const PACKAGE_NAME: &str = "com.promtuz.chat";

/// The client's SQLite databases, each opened and migrated on first use.
pub struct Stores {
    /// `None` keeps every database in memory.
    dir: Option<String>,
    files: String,
    identity: OnceLock<Mutex<Connection>>,
    contacts: OnceLock<Mutex<Connection>>,
    messages: OnceLock<Mutex<Connection>>,
    network: OnceLock<Mutex<Connection>>,
    outbox: OnceLock<Mutex<Connection>>,
    transfers: OnceLock<Mutex<Connection>>,
    mls: OnceLock<Arc<Mutex<Connection>>>,
}

impl Stores {
    /// `PROMTUZ_DATA_DIR` replaces the Android data dir for host runs.
    pub(crate) fn open_default() -> Self {
        // Tests reach storage through `test_support::ScopedCore`; nothing can be created here.
        if cfg!(test) {
            return Self::open("/dev/null".into(), "/dev/null".into());
        }
        match std::env::var("PROMTUZ_DATA_DIR") {
            Ok(dir) => Self::open(dir.clone(), dir),
            Err(_) => Self::open(
                format!("/data/data/{PACKAGE_NAME}/databases"),
                format!("/data/data/{PACKAGE_NAME}"),
            ),
        }
    }

    /// Databases in `dir`, large blobs under `<data>/files`.
    pub fn open(dir: String, data: String) -> Self {
        Self::new(Some(dir), format!("{data}/files"))
    }

    pub fn in_memory(data: String) -> Self {
        Self::new(None, format!("{data}/files"))
    }

    fn new(dir: Option<String>, files: String) -> Self {
        Self {
            dir,
            files,
            identity: OnceLock::new(),
            contacts: OnceLock::new(),
            messages: OnceLock::new(),
            network: OnceLock::new(),
            outbox: OnceLock::new(),
            transfers: OnceLock::new(),
            mls: OnceLock::new(),
        }
    }

    pub fn identity(&self) -> &Mutex<Connection> {
        self.identity.get_or_init(|| self.connect("identity", identity::migrate, &[]))
    }

    pub fn contacts(&self) -> &Mutex<Connection> {
        self.contacts.get_or_init(|| self.connect("contacts", peers::migrate, &["contacts"]))
    }

    pub fn messages(&self) -> &Mutex<Connection> {
        self.messages.get_or_init(|| self.connect("messages", messages::migrate, messages::WATCHED))
    }

    pub fn network(&self) -> &Mutex<Connection> {
        self.network.get_or_init(|| self.connect("network", network::migrate, &[]))
    }

    pub fn outbox(&self) -> &Mutex<Connection> {
        self.outbox.get_or_init(|| self.connect("outbox", outbox::migrate, &[]))
    }

    pub fn transfers(&self) -> &Mutex<Connection> {
        self.transfers
            .get_or_init(|| self.connect("transfers", crate::transfer::store::migrate, &["partials"]))
    }

    /// The one MLS connection. Its mutex serializes every MLS read and write, and an open storage
    /// `Operation` holds it for a whole OpenMLS operation.
    pub fn mls(&self) -> Arc<Mutex<Connection>> {
        self.mls.get_or_init(|| Arc::new(self.connect("mls", mls::migrate, &[]))).clone()
    }

    fn connect(
        &self, name: &str, migrate: fn(&mut Connection), watched: &[&str],
    ) -> Mutex<Connection> {
        let mut conn = match &self.dir {
            Some(dir) => Connection::open(db_path(dir, name)),
            None => Connection::open_in_memory(),
        }
        .expect("db open failed");
        migrate(&mut conn);
        if !watched.is_empty() {
            register_change_hook(&conn, watched);
        }
        Mutex::new(conn)
    }

    /// `files/<sub>` for large blobs, created on demand.
    pub fn files_dir(&self, sub: &str) -> String {
        let dir = format!("{}/{sub}", self.files);
        if !Path::new(&dir).is_dir() && fs::create_dir_all(&dir).is_err() {
            fatal("Failed to create files directory!");
        }
        dir
    }
}

fn db_path(dir: &str, file_name: &str) -> String {
    if !Path::new(dir).is_dir() && fs::create_dir_all(dir).is_err() {
        fatal("Failed to create database directory!");
    }
    format!("{dir}/{file_name}.db")
}

/// The app cannot run without its data directory. A test panics instead, naming the cause.
fn fatal(what: &str) -> ! {
    log::error!("{what}");
    if cfg!(test) {
        panic!("{what} A test reaches storage through test_support::ScopedCore.");
    }
    process::exit(1)
}

fn prepare(conn: &mut Connection, migrations: &Migrations) {
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    if cfg!(target_os = "android") {
        conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
        conn.pragma_update(None, "temp_store", "MEMORY").unwrap();
    }
    migrations.to_latest(conn).expect("db migration failed");
}

/// Calls the client's `on_db_changed` with `tables` after every commit on `conn`. The hook runs
/// with the connection locked, so the client may only wake a flow, never block or call into core.
pub(crate) fn register_change_hook(conn: &rusqlite::Connection, tables: &[&str]) {
    let tables: Vec<String> = tables.iter().map(|s| (*s).to_string()).collect();
    conn.commit_hook(Some(move || {
        if let Some(ev) = crate::state::core().events.get() {
            ev.on_db_changed(tables.clone());
        }
        false // observe only, never roll back the commit
    }));
}

/// Every row of `sql`, through the connection's statement cache. A row that fails to map fails the
/// whole call.
pub(crate) fn all<T>(
    conn: &Connection, sql: &str, params: impl Params,
    f: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    conn.prepare_cached(sql)?.query_map(params, f)?.collect()
}

pub(crate) fn one<T>(
    conn: &Connection, sql: &str, params: impl Params,
    f: impl FnOnce(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Option<T>> {
    conn.prepare_cached(sql)?.query_row(params, f).optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_database_migrates_from_empty_with_sound_foreign_keys() {
        for (name, migrate) in [
            ("identity", identity::migrate as fn(&mut Connection)),
            ("contacts", peers::migrate),
            ("messages", messages::migrate),
            ("network", network::migrate),
            ("outbox", outbox::migrate),
            ("mls", mls::migrate),
            ("transfers", crate::transfer::store::migrate),
        ] {
            let conn = crate::test_support::data::open(migrate);
            // A parent key that is not a primary or unique key fails here even with no rows.
            let broken: Vec<String> =
                all(&conn, "PRAGMA foreign_key_check", [], |r| r.get(0)).unwrap();
            assert!(broken.is_empty(), "{name}: {broken:?}");
        }
        for migrations in [
            identity::MIGRATIONS,
            peers::MIGRATIONS,
            messages::MIGRATIONS,
            network::MIGRATIONS,
            outbox::MIGRATIONS,
        ] {
            migrations.validate().unwrap();
        }
    }
}
