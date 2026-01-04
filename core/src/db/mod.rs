//! TODO: Minify SQL before executing

pub mod identity;
pub mod network;
pub mod peers;

use std::fs;
use std::path::Path;
use std::process;

pub use network::NETWORK_DB;

// pub use users::USERS_DB;
use crate::PACKAGE_NAME;

fn db(file_name: &'static str) -> String {
    let db_dir = format!("/data/data/{PACKAGE_NAME}/databases");
    let dir_path = Path::new(&db_dir);

    if !dir_path.is_dir() && fs::create_dir(dir_path).is_err() {
        log::error!("Failed to create database directory!");
        process::exit(1);
    }

    format!("{db_dir}/{file_name}.db")
}

/// TODO: maybe implement a proc-macro one day
macro_rules! from_row {
    ($ty:ident { $($field:ident),* $(,)? }) => {
        impl $ty {
            pub fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
                Ok(Self {
                    $(
                        $field: row.get(stringify!($field))?,
                    )*
                })
            }
        }
    };
}

pub(super) use from_row;

#[macro_export]
macro_rules! PRAGMA {
    ($conn:expr, $MIGRATIONS:expr) => {
        // Set PRAGMAs before migrations
        $conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        $conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        if cfg!(target_os = "android") {
            $conn.pragma_update(None, "synchronous", "NORMAL").unwrap();
            $conn.pragma_update(None, "temp_store", "MEMORY").unwrap();
        }

        // Migrations
        $MIGRATIONS.to_latest(&mut $conn).expect("db migration failed");
    };
}
