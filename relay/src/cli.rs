use std::path::PathBuf;

use clap::Parser;

const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PZ_GIT_SHA"), ")");

/// Promtuz relay node CLI.
#[derive(Parser, Debug)]
#[command(name = "pzrelay", version = VERSION, about = "Promtuz relay node")]
pub struct Cli {
    /// Path to the config file.
    #[arg(short, long, default_value = "/etc/promtuz/relay.toml")]
    pub config: PathBuf,
}

impl Cli {
    /// Parse argv (handles `--version` / `--help` and exits as clap does).
    pub fn get() -> Self {
        Self::parse()
    }
}
