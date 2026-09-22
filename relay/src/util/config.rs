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

    /// Optional P2P hole-punch assist block. Default **disabled**.
    #[serde(default)]
    pub assist: AssistConfig,

    /// Optional TURN server for calls. Default **disabled**.
    #[serde(default)]
    pub turn: TurnConfig,

    /// Optional logging block. Absent → info. `PZ_LOG` env overrides.
    #[serde(default)]
    pub log: LogConfig,
}

/// STUN echo + TURN bridge on the QUIC port (see [`crate::stunturn`]).
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct AssistConfig {
    /// Off by default: bridge tokens are unissued bearer secrets, so an
    /// enabled relay will forward datagrams for anyone who guesses one.
    #[serde(default)]
    pub enabled: bool,
}

/// A standard TURN server for calls on its own UDP port (see [`crate::turn`]).
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TurnConfig {
    #[serde(default)]
    pub enabled:   bool,
    /// UDP port to listen on. Default 3478, the TURN port.
    #[serde(default = "default_turn_port")]
    pub port:      u16,
    /// The address peers reach relayed traffic at: this host's public IP.
    /// Required when enabled, because a relay allocation is handed to the
    /// far end as `public_ip:port` and the relay cannot see its own NAT.
    pub public_ip: Option<std::net::IpAddr>,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { enabled: false, port: default_turn_port(), public_ip: None }
    }
}

fn default_turn_port() -> u16 {
    3478
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
            common::server::log::flush();
            std::process::exit(1);
        }

        if let Ok(raw) = fs::read_to_string(path) {
            match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    common::error!("Failed to parse config\n{err}");
                    common::server::log::flush();
                    process::exit(1);
                },
            }
        } else {
            common::error!("Failed to read config");
            common::server::log::flush();
            process::exit(1);
        }
    }
}
