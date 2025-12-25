//! TODO: Minify SQL before executing
//! TODO: Integrate rusqlite_migrations

use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::PACKAGE_NAME;

fn db(file_name: &'static str) -> String {
    format!("/data/data/{PACKAGE_NAME}/databases/{file_name}.db")
}

/// Connection to any sqlite network db
pub static NETWORK_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
    let db = Mutex::new(Connection::open(db("network")).expect("db open failed"));
    info!("DB: Network Database Connected");
    db
});

const RELAYS_SQL: &str = include_str!("../db/relays.sql");

pub fn initial_execute() -> anyhow::Result<()> {
    ////////////////////////
    //  NETWORK DATABASE  //
    ////////////////////////
    NETWORK_DB.lock().execute(RELAYS_SQL, ())?;

    Ok(())
}