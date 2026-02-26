use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

use common::node::config::NetworkConfig;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    // pub resolver: ResolverConfig,
}

impl AppConfig {
    pub fn load(cls: bool) -> Self {
        if cls {
            print!("\x1B[2J\x1B[1;1H");
            std::io::stdout().flush().ok();
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
