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
    /// Optional upload backend. Requires `STICKER_STORE` on the certificate.
    #[serde(default)]
    pub store:    Option<StoreConfig>,
}

#[derive(Deserialize, Debug)]
pub struct StoreConfig {
    /// Must match the resolver's entry for this bucket.
    pub id:      u16,
    /// Local ownership and quota ledger. Existing ownership is recovered
    /// from the stored manifest if this file is lost.
    pub db:      std::path::PathBuf,
    #[serde(flatten)]
    pub backend: BackendConfig,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum BackendConfig {
    /// A directory, for local runs: serve it with any static HTTP server and
    /// point the resolver's store row at that.
    Fs { dir: std::path::PathBuf },
    /// An S3-compatible bucket (R2). Path-style addressing: objects land at
    /// `{endpoint}/{bucket}/{key}`.
    S3 {
        endpoint:   String,
        bucket:     String,
        #[serde(default = "auto_region")]
        region:     String,
        access_key: String,
        secret_key: String,
    },
}

fn auto_region() -> String {
    "auto".into()
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
            common::server::log::flush();
            std::process::exit(1);
        }

        match fs::read_to_string(path) {
            Ok(raw) => match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    common::error!("parse config\n{err}");
                    common::server::log::flush();
                    process::exit(1);
                },
            },
            Err(_) => {
                common::error!("Failed to read config");
                common::server::log::flush();
                process::exit(1);
            },
        }
    }
}
