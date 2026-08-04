/// MUST BE USED AT STARTUP
/// NEVER USE AT RUNTIME
#[macro_export]
macro_rules! graceful {
    ($expr:expr, $msg:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                $crate::error!("{}: {}", $msg, e);
                // Logging is async; `exit` runs no destructors, so the record
                // dies in the queue unless it is drained here.
                $crate::server::log::flush();
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
