use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    #[serde(default)]
    pub log:     LogConfig,
    #[serde(default)]
    pub push:    PushConfig,
    /// Resolver seeds to register with, so relays can discover this gateway.
    /// Absent → the gateway runs but registers nowhere (undiscoverable).
    #[serde(default)]
    pub resolver: Option<NodeConfig>,
}

#[derive(Deserialize, Debug, Default)]
pub struct PushConfig {
    /// Path to the FCM service-account JSON. Absent → FCM dispatch is disabled
    /// (the gateway still runs; a wake for an FCM token is logged and dropped).
    pub fcm_service_account: Option<std::path::PathBuf>,
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

        match fs::read_to_string(path) {
            Ok(raw) => match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    common::error!("parse config\n{err}");
                    process::exit(1);
                },
            },
            Err(_) => {
                common::error!("Failed to read config");
                process::exit(1);
            },
        }
    }
}
