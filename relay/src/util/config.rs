use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process;

use common::node::config::NodeConfig;
use common::node::config::NodeSeed;
use serde::Deserialize;


#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub resolver: NodeConfig,
    #[serde(default)]
    pub dht: DhtConfig,
}

#[derive(Deserialize, Debug)]
pub struct NetworkConfig {
    /// Local Address of relay where endpoint will bind
    ///
    /// Not to be confused with public address
    pub address: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub root_ca_path: PathBuf,
}

#[derive(Deserialize, Debug, Clone)]
pub struct DhtConfig {
    pub bootstrap: Vec<NodeSeed>,
    pub bucket_size: usize,
    pub k: usize,
    pub alpha: usize,
    pub user_ttl_secs: u64,
    pub republish_secs: u64,
}

impl Default for DhtConfig {
    fn default() -> Self {
        DhtConfig {
            bootstrap: vec![],
            bucket_size: 20,
            k: 8,
            alpha: 3,
            user_ttl_secs: 300,
            republish_secs: 120,
        }
    }
}

impl AppConfig {
    pub fn load(cls: bool) -> Self {
        if cls {
            print!("\x1B[2J\x1B[1;1H");
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
