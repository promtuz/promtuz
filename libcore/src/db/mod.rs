use std::{fs, path::Path, process};

mod macros;

pub mod identity;
pub mod messages;
pub mod network;
pub mod peers;
pub mod utils;

static PACKAGE_NAME: &str = "com.promtuz.chat";

pub fn db(file_name: &'static str) -> String {
    let db_dir = format!("/data/data/{PACKAGE_NAME}/databases");
    let dir_path = Path::new(&db_dir);

    if !dir_path.is_dir() && fs::create_dir(dir_path).is_err() {
        log::error!("Failed to create database directory!");
        process::exit(1);
    }

    format!("{db_dir}/{file_name}.db")
}
