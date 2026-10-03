/// Startup only, never at runtime: it exits the process on error.
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

#[macro_export]
macro_rules! ret {
    ($expr:expr) => {
        match $expr {
            Some(v) => v,
            None => return,
        }
    };
}

/// The resolver's and gateway's `cli` module: `--config`, and `enroll` in place of the daemon. It
/// expands in the daemon's crate, so `VERSION` is that crate's.
#[macro_export]
macro_rules! daemon_cli {
    ($name:literal, $about:literal) => {
        pub const VERSION: &str =
            concat!(env!("CARGO_PKG_VERSION"), " (", env!("PZ_GIT_SHA"), ")");

        /// With no subcommand, runs the daemon.
        #[derive(clap::Parser, Debug)]
        #[command(name = concat!("pz", $name), version = VERSION, about = $about)]
        pub struct Cli {
            /// Path to the config file.
            #[arg(short, long, default_value = concat!("/etc/promtuz/", $name, ".toml"))]
            pub config: std::path::PathBuf,

            #[command(subcommand)]
            pub command: Option<Command>,
        }

        #[derive(clap::Subcommand, Debug)]
        pub enum Command {
            /// Print the CSR, then install a signed cert pasted on stdin.
            Enroll,
        }
    };
}
