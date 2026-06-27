use std::fmt;
use std::net::IpAddr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

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

    /// Restart the daemon in place when this config file changes. Default off
    /// — a bad edit otherwise risks an unwanted restart.
    #[serde(default)]
    pub watch_reload: bool,
}

/// Default QUIC ports, applied when a [`HostAddr`] in config omits one.
pub const DEFAULT_RESOLVER_PORT: u16 = 40433;
pub const DEFAULT_RELAY_PORT: u16 = 40432;

/// A `host[:port]` from config — either a literal IP or a DNS name, with an
/// optional port. Unlike [`SocketAddr`] it accepts hostnames; the name is
/// resolved and the default port applied lazily at dial time via
/// [`HostAddr::resolve`], so a moved box is followed by repointing DNS rather
/// than editing every config.
#[derive(Debug, Clone)]
pub struct HostAddr {
    host: Host,
    port: Option<u16>,
}

#[derive(Debug, Clone)]
enum Host {
    Ip(IpAddr),
    Name(String),
}

impl FromStr for HostAddr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // `IP:port` (incl. bracketed IPv6) → fixed socket address.
        if let Ok(sa) = s.parse::<SocketAddr>() {
            return Ok(Self { host: Host::Ip(sa.ip()), port: Some(sa.port()) });
        }
        // Bare IP, no port → default applied at resolve time.
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(Self { host: Host::Ip(ip), port: None });
        }
        // `name[:port]` — split on the last colon so the name stays intact.
        match s.rsplit_once(':') {
            Some((name, port)) if !name.is_empty() => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port in host address '{s}'"))?;
                Ok(Self { host: Host::Name(name.to_owned()), port: Some(port) })
            },
            _ if !s.is_empty() => Ok(Self { host: Host::Name(s.to_owned()), port: None }),
            _ => Err("empty host address".to_owned()),
        }
    }
}

impl fmt::Display for HostAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.host, self.port) {
            (Host::Ip(ip), Some(p)) => write!(f, "{}", SocketAddr::new(*ip, p)),
            (Host::Ip(ip), None) => write!(f, "{ip}"),
            (Host::Name(n), Some(p)) => write!(f, "{n}:{p}"),
            (Host::Name(n), None) => write!(f, "{n}"),
        }
    }
}

#[cfg(feature = "tokio")]
impl HostAddr {
    /// Resolve to a concrete [`SocketAddr`], applying `default_port` when the
    /// config omitted one. A literal IP returns immediately; a DNS name is
    /// looked up (first address wins). Call at dial time so reconnects pick up
    /// DNS changes.
    pub async fn resolve(&self, default_port: u16) -> std::io::Result<SocketAddr> {
        let port = self.port.unwrap_or(default_port);
        match &self.host {
            Host::Ip(ip) => Ok(SocketAddr::new(*ip, port)),
            Host::Name(name) => tokio::net::lookup_host((name.as_str(), port))
                .await?
                .next()
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("no addresses resolved for host '{name}'"),
                    )
                }),
        }
    }
}

#[serde_as]
#[derive(Deserialize, Debug, Clone)]
pub struct NodeSeed {
    pub key: NodeKey,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub addr: HostAddr,
}

/// Node Config
///
/// Can be either resolver or relay
#[derive(Deserialize, Debug)]
pub struct NodeConfig {
    pub seed: Vec<NodeSeed>,
}