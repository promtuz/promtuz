use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PZ_GIT_SHA"), ")");

/// Promtuz relay node CLI. With no subcommand, runs the daemon.
#[derive(Parser, Debug)]
#[command(name = "pzrelay", version = VERSION, about = "Promtuz relay node")]
pub struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "/etc/promtuz/relay.toml")]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Utility subcommands that run instead of the daemon.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Wipe the running relay's on-disk store (live; no restart).
    ClearDb,
    /// Print the CSR, then install a signed cert pasted on stdin.
    Enroll,
}
