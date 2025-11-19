use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub mod config;


pub fn systime() -> Duration {
    let now = SystemTime::now();
    now.duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0))
}

pub fn systime_sec() -> u64 {
    systime().as_secs()
}
