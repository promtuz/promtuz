use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub mod rocksdb;
pub mod config;

pub fn systime() -> Duration {
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0))
}