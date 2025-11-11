use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process;

use serde::Deserialize;
use url::Url;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub resolver: ResolverConfig,
}

#[derive(Deserialize, Debug)]
pub struct NetworkConfig {
    pub address: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub root_ca_path: PathBuf,
}

#[derive(Deserialize, Debug)]
pub struct ResolverConfig {
    pub seed: Vec<Url>,
}

impl AppConfig {
    pub fn load() -> Self {
        let path = env::args().nth(1).unwrap_or_else(|| "config.toml".into());
        let path = Path::new(&path);

        if !path.exists() {
            eprintln!("ERROR: config.toml not found: {}", path.display());
            std::process::exit(1);
        }

        if let Ok(raw) = fs::read_to_string(path) {
            match toml::from_str(&raw) {
                Ok(conf) => conf,
                Err(err) => {
                    eprintln!("ERROR: Failed to parse config : {:#?}", err);
                    process::exit(1);
                },
            }
        } else {
            eprintln!("ERROR: Failed to read config");
            process::exit(1);
        }
    }
}
