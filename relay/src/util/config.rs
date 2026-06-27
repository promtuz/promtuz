use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use serde::Deserialize;

use crate::dht::DhtConfig;


#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub resolver: NodeConfig,

    /// Optional DHT block. Absent / `enabled = false` keeps the relay on
    /// the pre-DHT code path. The default is **disabled**.
    #[serde(default)]
    pub dht: DhtConfig,

    /// Optional logging block. Absent → info. `PZ_LOG` env overrides.
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Deserialize, Debug, Default)]
pub struct LogConfig {
    /// trace|debug|info|warn|error. `PZ_LOG` env overrides. Default: info.
    pub level: Option<String>,
}

impl AppConfig {
    pub fn load(path: &Path, cls: bool) -> Self {
        if cls {
            print!("\x1B[2J\x1B[1;1H");
            std::io::stdout().flush().ok();
        }

        if !path.exists() {
            common::error!("config not found: {}", path.display());
            std::process::exit(1);
        }

        if let Ok(raw) = fs::read_to_string(path) {
            match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    common::error!("Failed to parse config\n{err}");
                    process::exit(1);
                },
            }
        } else {
            common::error!("Failed to read config");
            process::exit(1);
        }
    }
}
