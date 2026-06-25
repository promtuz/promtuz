use std::env;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::process;

use common::node::config::NodeConfig;
use serde::Deserialize;

use crate::dht::DhtConfig;


#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub resolver: NodeConfig,

    /// Optional DHT block. Absent / `enabled = false` keeps the relay on
    /// the pre-DHT code path — see `relay/src/dht/config.rs` and §10/§11.8
    /// of `misc/specs/DHT.md`. The default is **disabled**.
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

    /// PKCS#8 PEM private key used as the **TLS server key**. This is the
    /// material rustls hands to its TLS handshake signer; a key compromise
    /// here is bounded to the lifetime of the issued cert.
    pub key_path: PathBuf,

    /// PKCS#8 PEM Ed25519 private key used as this relay's **long-term
    /// identity key**, separate from the TLS server key. Anything signed
    /// "as this relay" (e.g. `RelayHello` to the resolver) uses this key.
    ///
    /// Kept separate from `key_path` so a TLS-layer compromise (memory
    /// disclosure in rustls/aws-lc-rs, a leaked cert key, …) does not
    /// silently turn into a permanent identity compromise.
    ///
    /// The file is auto-generated with `0o600` perms on first boot if it
    /// does not exist; treat it like an SSH host key thereafter.
    pub identity_key_path: PathBuf,

    pub root_ca_path: PathBuf,
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
