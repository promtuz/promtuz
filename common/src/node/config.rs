use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;
use serde_with::serde_as;

use crate::quic::id::NodeKey;

/// Network section of `config.toml` for both relay & resolver
#[derive(Deserialize, Debug)]
pub struct NetworkConfig {
    /// address on which quic endpoint will start
    pub address: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// root ca to verify outgoing/incoming quic connections
    pub root_ca_path: PathBuf,
}

#[serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct NodeSeed {
    pub key: NodeKey,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr: SocketAddr,
}

/// Node Config
///
/// Can be either resolver or relay
#[derive(Deserialize, Debug)]
pub struct NodeConfig {
    pub seed: Vec<NodeSeed>,
}