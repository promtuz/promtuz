use std::fs;
use std::net::SocketAddr;

use anyhow::Result;
use common::quic::id::derive_id;
use p256::SecretKey;

use crate::util::config::AppConfig;

/// contains p256 private & public key
#[derive(Debug, PartialEq, Eq)]
pub struct RelayKeys {
    pub secret: p256::SecretKey,
    pub public: p256::PublicKey,
}

impl RelayKeys {
    fn from_cfg(cfg: &AppConfig) -> Result<Self> {
        let sec = fs::read_to_string(&cfg.network.key_path)?;
        let secret = SecretKey::from_sec1_pem(&sec)?;

        Ok(Self { public: secret.public_key(), secret })
    }
}

/// Represents a single relay node running in the network.
///
/// It's *local identity* of the relay process,
/// not a message exchanged over the wire.
#[derive(Debug, PartialEq, Eq)]
pub struct Relay {
    /// Human readable relay id derived from public key
    pub id: String,

    pub keys: RelayKeys,

    /// Protocol version this instance is running with
    pub version: u16,
}

impl Relay {
    pub fn from_cfg(cfg: &AppConfig) -> Result<Self> {
        let keys = RelayKeys::from_cfg(cfg)?;
        let id = derive_id(&keys.public);

        println!("RELAY: Booting with ID({})", id);

        Ok(Self { id, keys, version: common::PROTOCOL_VERSION })
    }
}
