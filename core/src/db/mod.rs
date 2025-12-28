//! TODO: Minify SQL before executing
//! TODO: Integrate rusqlite_migrations

use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;

pub mod identity;
pub mod network;
pub mod peers;

pub use network::NETWORK_DB;

// pub use users::USERS_DB;
use crate::PACKAGE_NAME;

fn db(file_name: &'static str) -> String {
    format!("/data/data/{PACKAGE_NAME}/databases/{file_name}.db")
}

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
