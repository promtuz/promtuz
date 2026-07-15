pub mod contact;
pub mod identity;
pub mod idqr;
pub mod backup;
pub mod message;
pub mod reaction;
pub mod recovery;
pub mod relay;
pub mod seen;

use std::str::FromStr;

use anyhow::Result;
use anyhow::anyhow;
use common::node::config::HostAddr;
use common::quic::id::NodeKey;

#[derive(Debug)]
pub struct ResolverSeeds {}

#[derive(Debug, Clone)]
pub struct ResolverSeed {
    pub key: NodeKey,
    /// `host[:port]` — IP or DNS name; the port defaults to
    /// [`common::node::config::DEFAULT_RESOLVER_PORT`] at dial time.
    pub addr: HostAddr,
}

impl ResolverSeeds {
    /// `<IPK_HEX>::<host[:port]>` — one per line. `host` may be an IP or a
    /// DNS name; the port is optional and defaults to the resolver port
    /// (40433), resolved lazily at dial time (so a moved box is followed by
    /// repointing DNS, not re-flashing clients).
    ///
    /// Example
    ///
    /// ```txt
    /// 55ECA4054C27FF7E8D613F876CBED9C9AC72F7737AF520511663EBA4F6CE1D1B::resolver.promtuz.dev
    /// ```
    // The signature returns `Vec<ResolverSeed>` rather than `Self`,
    // so this is genuinely a multi-line parser, not a `FromStr`
    // implementation candidate.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Vec<ResolverSeed>> {
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
}
