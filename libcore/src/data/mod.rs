pub mod identity;
pub mod idqr;
pub mod relay;

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Result;
use anyhow::anyhow;
use common::proto::ResolverId;
use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ResolverSeeds {}

#[derive(Deserialize, Debug)]
pub struct ResolverSeed {
    pub id: ResolverId,
    pub addr: SocketAddr,
}

impl ResolverSeeds {
    /// Example
    ///
    /// ```txt
    /// I2DRYSCOMXODBJ47::192.168.100.2:4433
    /// ```
    pub fn from_str(text: &str) -> Result<Vec<ResolverSeed>> {
        let mut seeds = vec![];

        for (index, line) in text.lines().enumerate() {
            let (id, addr) = line
                .split_once("::")
                .ok_or_else(|| anyhow!("Invalid seed syntax on line {}", index + 1))?;

            let id = ResolverId::from_str(id).map_err(|e| anyhow!(e))?;
            let addr = SocketAddr::from_str(addr)?;

            seeds.push(ResolverSeed { id, addr });
        }

        Ok(seeds)
    }
}
