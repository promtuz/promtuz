pub mod contact;
pub mod identity;
pub mod idqr;
pub mod message;
pub mod relay;

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Result;
use anyhow::anyhow;
use common::quic::id::NodeKey;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ResolverSeeds {}

#[derive(Deserialize, Debug)]
pub struct ResolverSeed {
    pub key: NodeKey,
    pub addr: SocketAddr,
}

impl ResolverSeeds {
    /// `<IPK_HEX>::<IP>:<PORT>`
    ///
    /// Example
    ///
    /// ```txt
    /// A038F54EC3EBC391F423236E0091413C7275EFEDC65E89D3BFF9DF055FEFE4CC::192.168.100.2:4433
    /// ```
    pub fn from_str(text: &str) -> Result<Vec<ResolverSeed>> {
        let mut seeds = vec![];

        for (index, line) in text.lines().enumerate() {
            let (key, addr) = line
                .split_once("::")
                .ok_or_else(|| anyhow!("Invalid seed syntax on line {}", index + 1))?;

            let key = NodeKey::new(hex::decode(key)?)?;
            let addr = SocketAddr::from_str(addr)?;

            seeds.push(ResolverSeed { key, addr });
        }

        Ok(seeds)
    }
}
