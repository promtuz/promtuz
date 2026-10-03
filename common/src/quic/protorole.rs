use std::{fmt, str::FromStr};

use quinn::{Connection, crypto::rustls::HandshakeData};
use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtoRole {
    Resolver,
    Relay,
    Peer,
    Client,
}

impl ProtoRole {
    pub fn alpn(self) -> String {
        match self {
            ProtoRole::Resolver => format!("resolver/{PROTOCOL_VERSION}"),
            ProtoRole::Relay => format!("relay/{PROTOCOL_VERSION}"),
            ProtoRole::Peer => format!("peer/{PROTOCOL_VERSION}"),
            ProtoRole::Client => format!("client/{PROTOCOL_VERSION}"),
        }
    }

    /// Accepts `role` or `role/version` and ignores the version.
    pub fn from_alpn(s: &str) -> Option<Self> {
        let role = s.split('/').next()?;

        match role {
            "resolver" => Some(ProtoRole::Resolver),
            "relay" => Some(ProtoRole::Relay),
            "peer" => Some(ProtoRole::Peer),
            "client" => Some(ProtoRole::Client),
            _ => None,
        }
    }

    pub fn from_conn(conn: &Connection) -> Option<Self> {
        let any = conn.handshake_data()?;
        let hs = any.downcast_ref::<HandshakeData>()?;

        let alpn_bytes = hs.protocol.as_ref()?;

        let alpn_str = std::str::from_utf8(alpn_bytes).ok()?;

        alpn_str.parse::<ProtoRole>().ok()
    }
}

impl fmt::Display for ProtoRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.alpn())
    }
}

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
            ProtoRole::Relay => "relay",
            ProtoRole::Peer => "peer",
            ProtoRole::Client => "client",
        }
    }
}
