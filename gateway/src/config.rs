use common::node::config::NetworkConfig;
use common::node::config::NodeConfig;
use common::server::log::LogConfig;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub network: NetworkConfig,
    #[serde(default)]
    pub log:     LogConfig,
    #[serde(default)]
    pub push:    PushConfig,
    /// Resolver seeds to register with. Without them, relays cannot discover this gateway.
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

#[derive(Deserialize, Debug)]
pub struct PushConfig {
    /// Persistent device registrations. systemd supplies STATE_DIRECTORY.
    #[serde(default = "default_push_db")]
    pub db: std::path::PathBuf,
    /// FCM service-account JSON. Without it, wakes for FCM tokens are logged and dropped.
    pub fcm_service_account: Option<std::path::PathBuf>,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self { db: default_push_db(), fcm_service_account: None }
    }
}

fn default_push_db() -> std::path::PathBuf {
    std::env::var_os("STATE_DIRECTORY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| ".".into())
        .join("push.db")
}
