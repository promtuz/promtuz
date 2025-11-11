use std::{fmt, str::FromStr};

use quinn::{Connection, crypto::rustls::HandshakeData};
use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtoRole {
    Resolver,
    Node,
    Peer,
    Client,
}

impl ProtoRole {
    /// Return this role as an ALPN string, including version.
    ///
    /// Example: `"node/1"`
    pub fn alpn(self) -> String {
        match self {
            ProtoRole::Resolver => format!("resolver/{PROTOCOL_VERSION}"),
            ProtoRole::Node => format!("node/{PROTOCOL_VERSION}"),
            ProtoRole::Peer => format!("peer/{PROTOCOL_VERSION}"),
            ProtoRole::Client => format!("client/{PROTOCOL_VERSION}"),
        }
    }

    /// Convert ALPN string to ProtoRole.
    /// Accepts `"role"` or `"role/version"`.
    pub fn from_alpn(s: &str) -> Option<Self> {
        let role = s.split('/').next()?; // ignore version for now

        match role {
            "resolver" => Some(ProtoRole::Resolver),
            "node" => Some(ProtoRole::Node),
            "peer" => Some(ProtoRole::Peer),
            "client" => Some(ProtoRole::Client),
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

        // Convert &str → ProtoRole
        alpn_str.parse::<ProtoRole>().ok()
    }
}

impl fmt::Display for ProtoRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.alpn())
    }
}

/// Allows: `"node/1".parse::<ProtoRole>()?`
impl FromStr for ProtoRole {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ProtoRole::from_alpn(s).ok_or(())
    }
}

impl AsRef<str> for ProtoRole {
    fn as_ref(&self) -> &str {
        match self {
            ProtoRole::Resolver => "resolver",
            ProtoRole::Node => "node",
            ProtoRole::Peer => "peer",
            ProtoRole::Client => "client",
        }
    }
}
