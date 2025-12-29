pub mod relay;
pub mod identity;
pub mod idqr;

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ResolverSeeds {
    pub seeds: Vec<ResolverSeed>,
}

#[derive(Deserialize, Debug)]
pub struct ResolverSeed {
    pub id: String,
    pub host: String,
    pub port: u16,
}
