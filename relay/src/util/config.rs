use std::path::PathBuf;

use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use serde::Deserialize;

use crate::dht::DhtConfig;

fn default_control_socket() -> PathBuf {
    // The packaged relay.toml sets /run/pzrelay; this default is a per-user path for local runs.
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

    #[serde(default = "default_control_socket")]
    pub control_socket: PathBuf,

    #[serde(default)]
    pub dht: DhtConfig,

    #[serde(default)]
    pub assist: AssistConfig,

    #[serde(default)]
    pub turn: TurnConfig,

    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct AssistConfig {
    /// Off by default: bridge tokens are unissued bearer secrets, so an
    /// enabled relay will forward datagrams for anyone who guesses one.
    #[serde(default)]
    pub enabled: bool,
    /// Authenticated bridges over the TLS fallback listener, separate from the UDP bearer tokens.
    #[serde(default)]
    pub tcp_enabled: bool,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TurnConfig {
    #[serde(default)]
    pub enabled:   bool,
    #[serde(default = "default_turn_port")]
    pub port:      u16,
    /// Required when enabled: allocations are advertised as `public_ip:port`, and the relay
    /// cannot see its own NAT.
    pub public_ip: Option<std::net::IpAddr>,
    /// Also relay to loopback, private and link-local peers. Only for a test
    /// cluster on one network.
    #[serde(default)]
    pub allow_local_peers: bool,
}

impl Default for TurnConfig {
    fn default() -> Self {
        Self { enabled: false, port: default_turn_port(), public_ip: None, allow_local_peers: false }
    }
}

fn default_turn_port() -> u16 {
    3478
}

#[derive(Deserialize, Debug, Default)]
pub struct LogConfig {
    /// trace|debug|info|warn|error. `PZ_LOG` env overrides. Default: info.
    pub level: Option<String>,

    #[serde(default)]
    pub dht: bool,
}
