use std::fmt;
use std::net::IpAddr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use serde::Deserialize;
use serde_with::serde_as;

use crate::quic::id::NodeKey;

#[derive(Deserialize, Debug)]
pub struct NetworkConfig {
    pub address: SocketAddr,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub root_ca_path: PathBuf,

    /// Re-execs the daemon in place when this config file changes.
    #[serde(default)]
    pub watch_reload: bool,

    /// Accepts phone connections over TLS/TCP on the QUIC port number. Nodes stay on QUIC.
    #[serde(default)]
    pub tcp_fallback: bool,
}

impl NetworkConfig {
    /// `0.0.0.0` becomes `::` so quinn opens a dual-stack socket: a v4 socket cannot dial a peer
    /// that resolves to IPv6. A specific IP keeps its family.
    pub fn bind_addr(&self) -> SocketAddr {
        match self.address {
            SocketAddr::V4(a) if a.ip().is_unspecified() => {
                (Ipv6Addr::UNSPECIFIED, a.port()).into()
            },
            other => other,
        }
    }
}

pub const DEFAULT_RESOLVER_PORT: u16 = 40433;
pub const DEFAULT_RELAY_PORT: u16 = 40432;
pub const DEFAULT_GATEWAY_PORT: u16 = 40434;

/// A `host[:port]` from config. Names resolve at dial time, so a moved box is followed by
/// repointing DNS rather than editing every config.
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
        if let Ok(sa) = s.parse::<SocketAddr>() {
            return Ok(Self { host: Host::Ip(sa.ip()), port: Some(sa.port()) });
        }
        if let Ok(ip) = s.parse::<IpAddr>() {
            return Ok(Self { host: Host::Ip(ip), port: None });
        }
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
    /// The first resolved address wins. Call at dial time so reconnects pick up DNS changes.
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

#[derive(Deserialize, Debug)]
pub struct NodeConfig {
    pub seed: Vec<NodeSeed>,
}
