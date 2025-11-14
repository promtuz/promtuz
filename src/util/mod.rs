use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub mod config;

pub fn systime_sec() -> u64 {
    let now = SystemTime::now();
    let since_the_epoch = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::from_secs(0));

    since_the_epoch.as_secs()
}

#[macro_export]
macro_rules! graceful {
    ($expr:expr, $msg:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{} {}", $msg, e);
                std::process::exit(1);
            },
        }
    };
}

/// Use to early return
#[macro_export]
macro_rules! ret {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => return,
        }
    };
}
