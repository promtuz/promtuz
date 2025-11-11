use std::{fmt, str::FromStr};

use quinn::{Connection, crypto::rustls::HandshakeData};
use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Resolver,
    Node,
    Peer,
    Client,
}

impl Role {
    /// Return this role as an ALPN string, including version.
    ///
    /// Example: `"node/1"`
    pub fn alpn(self) -> String {
        match self {
            Role::Resolver => format!("resolver/{PROTOCOL_VERSION}"),
            Role::Node => format!("node/{PROTOCOL_VERSION}"),
            Role::Peer => format!("peer/{PROTOCOL_VERSION}"),
            Role::Client => format!("client/{PROTOCOL_VERSION}"),
        }
    }

    /// Convert ALPN string to Role.
    /// Accepts `"role"` or `"role/version"`.
    pub fn from_alpn(s: &str) -> Option<Self> {
        let role = s.split('/').next()?; // ignore version for now

        match role {
            "resolver" => Some(Role::Resolver),
            "node" => Some(Role::Node),
            "peer" => Some(Role::Peer),
            "client" => Some(Role::Client),
            _ => None,
        }
    }

    pub fn from_conn(conn: &Connection) -> Option<Self> {
        // Get handshake data
        let any = conn.handshake_data()?;
        let hs = any.downcast_ref::<HandshakeData>()?;

        // hs.protocol is Option<Vec<u8>>
        let alpn_bytes = hs.protocol.as_ref()?;

        // Convert &[u8] → &str
        let alpn_str = std::str::from_utf8(alpn_bytes).ok()?;

        // Convert &str → Role
        alpn_str.parse::<Role>().ok()
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.alpn())
    }
}

/// Allows: `"node/1".parse::<Role>()?`
impl FromStr for Role {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Role::from_alpn(s).ok_or(())
    }
}

impl AsRef<str> for Role {
    fn as_ref(&self) -> &str {
        match self {
            Role::Resolver => "resolver",
            Role::Node => "node",
            Role::Peer => "peer",
            Role::Client => "client",
        }
    }
}
