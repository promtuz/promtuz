pub mod contact;
pub mod conversation;
pub mod identity;
pub mod idqr;
pub mod backup;
pub mod media;
pub mod message;
pub mod receipts;
pub mod app_prefs;
pub mod peer_avatar;
pub mod peer_profile;
pub mod group_picture;
pub mod peer_name;
pub mod reaction;
pub mod recovery;
pub mod relay;
pub mod seen;
pub mod stickers;

use std::str::FromStr;

use anyhow::Result;
use anyhow::anyhow;
use common::node::config::HostAddr;
use common::quic::id::NodeKey;

#[derive(Debug, Clone)]
pub struct ResolverSeed {
    pub key: NodeKey,
    pub addr: HostAddr,
}

/// One `<IPK_HEX>::<host[:port]>` per line; a DNS host resolves at dial time.
pub fn parse_seeds(text: &str) -> Result<Vec<ResolverSeed>> {
    let mut seeds = vec![];

    for (index, line) in text.lines().enumerate() {
        let (key, addr) = line
            .split_once("::")
            .ok_or_else(|| anyhow!("Invalid seed syntax on line {}", index + 1))?;

        let key = NodeKey::new(hex::decode(key)?)?;
        let addr = HostAddr::from_str(addr)
            .map_err(|e| anyhow!("invalid resolver addr on line {}: {e}", index + 1))?;

        seeds.push(ResolverSeed { key, addr });
    }

    Ok(seeds)
}
