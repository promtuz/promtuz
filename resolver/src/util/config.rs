use common::node::config::NetworkConfig;
use common::server::log::LogConfig;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    #[serde(default)]
    pub log: LogConfig,
    /// Served by `GetStores`. Store ids are permanent; change `base_url` to move one.
    #[serde(default)]
    pub store: Vec<StoreEntry>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct StoreEntry {
    pub id: u16,
    /// Scheme + host (+ port). A trailing slash is tolerated and stripped.
    pub base_url: String,
}
