#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        println!(
            "\x1b[48;5;235m\x1b[38;5;39m DEBUG \x1b[0m\x1b[48;5;235m{} \x1b[0m",
            format!($($arg)*)
        );
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        println!(
            "\x1b[48;5;236m\x1b[38;5;34m INFO  \x1b[0m\x1b[48;5;236m{} \x1b[0m",
            format!($($arg)*)
        );
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        println!(
            "\x1b[48;5;58m\x1b[38;5;220m WARN  \x1b[0m\
\x1b[48;5;58m\x1b[38;5;15m{} \x1b[0m",
            format!($($arg)*)
        );
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        eprintln!(
            "\x1b[48;5;52m\x1b[38;5;196m ERROR \x1b[0m\
\x1b[48;5;52m\x1b[38;5;15m{} \x1b[0m",
            format!($($arg)*)
        );
    }};
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        println!(
            "\x1b[48;5;234m\x1b[38;5;244m TRACE \x1b[0m\
\x1b[48;5;234m\x1b[38;5;245m{} \x1b[0m",
            format!($($arg)*)
        );
    }};
}
