use std::sync::atomic::{AtomicU8, Ordering};

/// Severity for the server log macros, ordered low→high. The active
/// threshold ([`init`]) suppresses anything below it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    Trace = 0,
    Debug = 1,
    Info = 2,
    Warn = 3,
    Error = 4,
}

/// Active threshold; defaults to Info until [`init`] runs.
static LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// True if `level` should be emitted at the current threshold.
#[inline]
pub fn enabled(level: Level) -> bool {
    (level as u8) >= LEVEL.load(Ordering::Relaxed)
}

fn parse(s: &str) -> Option<Level> {
    match s.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(Level::Trace),
        "debug" => Some(Level::Debug),
        "info" => Some(Level::Info),
        "warn" | "warning" => Some(Level::Warn),
        "error" => Some(Level::Error),
        _ => None,
    }
}

/// Resolve the threshold: `PZ_LOG` env wins, then the config value, else Info.
pub fn init(config_level: Option<&str>) {
    let env_level = std::env::var("PZ_LOG").ok();
    let chosen = env_level
        .as_deref()
        .and_then(parse)
        .or_else(|| config_level.and_then(parse))
        .unwrap_or(Level::Info);
    LEVEL.store(chosen as u8, Ordering::Relaxed);
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Debug) {
            println!(
                "\x1b[48;5;235m\x1b[38;5;39m DEBUG \x1b[0m\x1b[48;5;235m{} \x1b[0m",
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Info) {
            println!(
                "\x1b[48;5;236m\x1b[38;5;34m INFO  \x1b[0m\x1b[48;5;236m{} \x1b[0m",
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Warn) {
            println!(
                "\x1b[48;5;58m\x1b[38;5;220m WARN  \x1b[0m\
\x1b[48;5;58m\x1b[38;5;15m{} \x1b[0m",
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Error) {
            eprintln!(
                "\x1b[48;5;52m\x1b[38;5;196m ERROR \x1b[0m\
\x1b[48;5;52m\x1b[38;5;15m{} \x1b[0m",
                format!($($arg)*)
            );
        }
    }};
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Trace) {
            println!(
                "\x1b[48;5;234m\x1b[38;5;244m TRACE \x1b[0m\
\x1b[48;5;234m\x1b[38;5;245m{} \x1b[0m",
                format!($($arg)*)
            );
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_ordering_gates_correctly() {
        LEVEL.store(Level::Warn as u8, Ordering::Relaxed);
        assert!(!enabled(Level::Info));
        assert!(enabled(Level::Warn));
        assert!(enabled(Level::Error));
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(parse("DEBUG"), Some(Level::Debug));
        assert_eq!(parse("nope"), None);
    }
}
