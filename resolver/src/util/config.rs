use std::env;
use std::fs;
use std::io::stdout;
use std::path::Path;
use std::process;

use common::node::config::NetworkConfig;
// use common::node::config::ResolverConfig;
use crossterm::execute;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    // pub resolver: ResolverConfig,
}

impl AppConfig {
    pub fn load(cls: bool) -> Self {
        if cls {
            _ = execute!(stdout(), Clear(ClearType::All));
        }

        let path = env::args().nth(1).unwrap_or_else(|| "config.toml".into());
        let path = Path::new(&path);

        if !path.exists() {
            common::error!("config.toml not found: {}", path.display());
            std::process::exit(1);
        }

        if let Ok(raw) = fs::read_to_string(path) {
            match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    common::error!("parse config\n{err}");
                    process::exit(1);
                },
            }
        } else {
            common::error!("Failed to read config");
            process::exit(1);
        }
    }
}
