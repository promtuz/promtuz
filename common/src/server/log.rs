use std::io::BufWriter;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;

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

/// Lines buffered before [`emit`] starts dropping. Sized so a burst from one
/// remote peer cannot grow the process, and dropping beats blocking a reactor
/// thread on a `write(2)`.
const QUEUE_CAPACITY: usize = 8192;

enum Record {
    Line(Level, String),
    Barrier(mpsc::SyncSender<()>),
}

static SINK: OnceLock<mpsc::SyncSender<Record>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn sink() -> &'static mpsc::SyncSender<Record> {
    SINK.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("pz-log".into())
            .spawn(move || writer_loop(rx))
            .expect("spawning the log writer thread");
        tx
    })
}

fn write_record(out: &mut impl Write, err: &mut impl Write, record: Record) {
    match record {
        Record::Line(level, line) => {
            let sink: &mut dyn Write = if level == Level::Error { err } else { out };
            let _ = writeln!(sink, "{line}");
        },
        Record::Barrier(ack) => {
            let _ = out.flush();
            let _ = err.flush();
            let _ = ack.try_send(());
        },
    }
}

fn writer_loop(rx: mpsc::Receiver<Record>) {
    let mut out = BufWriter::new(std::io::stdout());
    let mut err = BufWriter::new(std::io::stderr());
    while let Ok(record) = rx.recv() {
        write_record(&mut out, &mut err, record);
        while let Ok(record) = rx.try_recv() {
            write_record(&mut out, &mut err, record);
        }
        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            let _ = writeln!(err, "{dropped} log lines dropped: writer could not keep up");
        }
        let _ = out.flush();
        let _ = err.flush();
    }
}

fn enqueue(tx: &mpsc::SyncSender<Record>, record: Record) {
    if tx.try_send(record).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Hand a formatted line to the writer thread. Never blocks: a full queue
/// drops the line and bumps the counter the writer reports on its next pass.
pub fn emit(level: Level, line: String) {
    enqueue(sink(), Record::Line(level, line));
}

/// Block until everything emitted so far has reached stdout/stderr. Call
/// before a process exits, otherwise the tail of the queue dies with it.
pub fn flush() {
    let (ack, done) = mpsc::sync_channel(1);
    if sink().send(Record::Barrier(ack)).is_ok() {
        let _ = done.recv_timeout(std::time::Duration::from_secs(2));
    }
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Debug) {
            $crate::server::log::emit(
                $crate::server::log::Level::Debug,
                format!(
                    "\x1b[48;5;235m\x1b[38;5;39m DEBUG \x1b[0m\x1b[48;5;235m{} \x1b[0m",
                    format!($($arg)*)
                ),
            );
        }
    }};
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Info) {
            $crate::server::log::emit(
                $crate::server::log::Level::Info,
                format!(
                    "\x1b[48;5;236m\x1b[38;5;34m INFO  \x1b[0m\x1b[48;5;236m{} \x1b[0m",
                    format!($($arg)*)
                ),
            );
        }
    }};
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Warn) {
            $crate::server::log::emit(
                $crate::server::log::Level::Warn,
                format!(
                    "\x1b[48;5;58m\x1b[38;5;220m WARN  \x1b[0m\
\x1b[48;5;58m\x1b[38;5;15m{} \x1b[0m",
                    format!($($arg)*)
                ),
            );
        }
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Error) {
            $crate::server::log::emit(
                $crate::server::log::Level::Error,
                format!(
                    "\x1b[48;5;52m\x1b[38;5;196m ERROR \x1b[0m\
\x1b[48;5;52m\x1b[38;5;15m{} \x1b[0m",
                    format!($($arg)*)
                ),
            );
        }
    }};
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{
        if $crate::server::log::enabled($crate::server::log::Level::Trace) {
            $crate::server::log::emit(
                $crate::server::log::Level::Trace,
                format!(
                    "\x1b[48;5;234m\x1b[38;5;244m TRACE \x1b[0m\
\x1b[48;5;234m\x1b[38;5;245m{} \x1b[0m",
                    format!($($arg)*)
                ),
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

    #[test]
    fn a_full_queue_drops_instead_of_blocking() {
        let (tx, _rx) = mpsc::sync_channel(1);
        DROPPED.store(0, Ordering::Relaxed);
        for i in 0..16 {
            enqueue(&tx, Record::Line(Level::Info, format!("line {i}")));
        }
        assert_eq!(DROPPED.load(Ordering::Relaxed), 15);
    }

    #[test]
    fn errors_go_to_stderr_and_the_rest_to_stdout() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        write_record(&mut out, &mut err, Record::Line(Level::Info, "hello".into()));
        write_record(&mut out, &mut err, Record::Line(Level::Error, "boom".into()));
        assert_eq!(out, b"hello\n");
        assert_eq!(err, b"boom\n");
    }
}
