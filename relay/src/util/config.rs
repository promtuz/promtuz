use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process;

use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use serde::Deserialize;

use crate::dht::DhtConfig;

fn default_control_socket() -> PathBuf {
    // Deployed sets this explicitly (packaged relay.toml → /run/pzrelay via the
    // unit's RuntimeDirectory). This default covers a local, no-config run: a
    // per-user, user-writable dir the daemon and client resolve identically.
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("pzrelay-control.sock");
    }
    PathBuf::from("/tmp/pzrelay-control.sock")
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub resolver: NodeConfig,

    /// Unix control socket for `pzrelay clear-db` (and future subcommands).
    /// Default matches the packaged unit's `RuntimeDirectory=pzrelay`; set a
    /// user-writable path for a local run outside systemd.
    #[serde(default = "default_control_socket")]
    pub control_socket: PathBuf,

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

    /// `[log] dht = true` unhides the DHT bootstrap/routing chatter.
    #[serde(default)]
    pub dht: bool,
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
