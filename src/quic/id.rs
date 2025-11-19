use std::{fmt, str::FromStr};

use data_encoding::BASE32_NOPAD;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::graceful;

/// Generate a compact, human-safe ID from a public key.
pub fn derive_id(pubkey: &p256::PublicKey) -> NodeId {
    // Use SEC1 uncompressed bytes (standard, same as OpenSSL)
    let encoded = pubkey.to_encoded_point(false);
    let hash = blake3::hash(encoded.as_bytes());

    // Use first 10 bytes → ~16 chars of Base32 (short + unique)
    let short = &hash.as_bytes()[..10];
    NodeId::from_bytes(graceful!(short.try_into(), "reality has clearly sprung a memory leak"))
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId([u8; 10]);

impl NodeId {
    pub const LEN: usize = 10;

    pub fn from_bytes(b: [u8; 10]) -> Self {
        NodeId(b)
    }

    pub fn as_bytes(&self) -> &[u8; 10] {
        &self.0
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let enc = BASE32_NOPAD.encode(&self.0);
        write!(f, "{enc}")
    }
}

impl FromStr for NodeId {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let decoded = BASE32_NOPAD.decode(s.as_bytes()).map_err(|_| "bad base32")?;
        if decoded.len() != Self::LEN {
            return Err("wrong length");
        }
        let mut arr = [0u8; Self::LEN];
        arr.copy_from_slice(&decoded);
        Ok(NodeId(arr))
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for NodeId {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&self.to_string())
    }
}