// use log::info;
// use once_cell::sync::Lazy;
// use parking_lot::Mutex;
// use rusqlite::Connection;
// use rusqlite_migration::M;
// use rusqlite_migration::Migrations;

// use crate::db::db;

// const MIGRATION_ARRAY: &[M] = &[M::up(
//     "CREATE TABLE users (
//               key TEXT PRIMARY KEY,
//               name TEXT NOT NULL
//           );",
// )];
// const MIGRATIONS: Migrations = Migrations::from_slice(MIGRATION_ARRAY);

// /// users :-
// /// 
// /// - identity public key
// /// - verification key
// /// - link to [keystore]?
// /// - name
// /// 
// /// keystore :-
// /// - contains ephemeral keys and their links
// /// 
// /// for eg.
// /// - my ephemeral keypair is stored in a sep. table named something,
// ///   that entry will be linked with lets say a keystore entry where it has the id of that something table and peer ID and peer 
// pub static USERS_DB: Lazy<Mutex<Connection>> = Lazy::new(|| {
//     let db = Mutex::new(Connection::open(db("users")).expect("db open failed"));
//     info!("DB: Users Database Connected");
//     db
// });
