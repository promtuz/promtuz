/// MUST BE USED AT STARTUP
/// NEVER USE AT RUNTIME
#[macro_export]
macro_rules! graceful {
    ($expr:expr, $msg:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                $crate::error!("{} {}", $msg, e);
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
