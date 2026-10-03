use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PZ_GIT_SHA"), ")");

/// Promtuz push gateway CLI. With no subcommand, runs the daemon.
#[derive(Parser, Debug)]
#[command(name = "pzgateway", version = VERSION, about = "Promtuz push gateway")]
pub struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "/etc/promtuz/gateway.toml")]
    pub config: PathBuf,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Print the CSR, then install a signed cert pasted on stdin.
    Enroll,
}
