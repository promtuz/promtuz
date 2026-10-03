//! Stable public identifiers, independent of transport and socket ownership.
use std::fmt;
use std::str::FromStr;

use anyhow::Result;
use data_encoding::BASE32_NOPAD;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Visitor;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BaseId<const N: usize>([u8; N]);

impl<const N: usize> BaseId<N> {
    pub const LEN: usize = N;

    pub fn from_bytes(b: [u8; N]) -> Self {
        Self(b)
    }

    pub fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }
}

impl<const N: usize> fmt::Display for BaseId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let enc = BASE32_NOPAD.encode(&self.0);
        write!(f, "{enc}")
    }
}

impl<const N: usize> fmt::Debug for BaseId<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<const N: usize> FromStr for BaseId<N> {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let decoded = BASE32_NOPAD.decode(s.as_bytes()).map_err(|_| "bad base32")?;
        if decoded.len() != N {
            return Err("wrong length");
        }
        let mut arr = [0u8; N];
        arr.copy_from_slice(&decoded);
        Ok(Self(arr))
    }
}

impl<const N: usize> Serialize for BaseId<N> {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        s.serialize_str(&self.to_string())
    }
}

impl<'de, const N: usize> Deserialize<'de> for BaseId<N> {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

pub type NodeId = BaseId<32>;

impl NodeId {
    /// Full-width BLAKE3, so relay NodeIds and user IPKs share one 256-bit XOR keyspace.
    pub fn new<K: AsRef<[u8]>>(key: K) -> Self {
        let hash = blake3::hash(key.as_ref());
        Self::from_bytes(*hash.as_bytes())
    }
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeKey([u8; 32]);

impl NodeKey {
    pub const LEN: usize = 32;

    pub fn new<K: AsRef<[u8]>>(key: K) -> Result<Self> {
        let key = key.as_ref();
        Ok(Self(key.try_into()?))
    }

    pub fn id(&self) -> NodeId {
        self.derive_id()
    }

    pub fn key(&self) -> String {
        hex::encode_upper(self.0)
    }

    #[inline]
    fn derive_id(&self) -> NodeId {
        let hash = blake3::hash(&self.0);
        NodeId::from_bytes(*hash.as_bytes())
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for NodeKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NodeKeyVisitor;

        impl<'de> Visitor<'de> for NodeKeyVisitor {
            type Value = NodeKey;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a 32-byte array or hex string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<NodeKey, E> {
                let bytes = hex::decode(v).map_err(E::custom)?;
                let arr: [u8; 32] = bytes.try_into().map_err(|_| E::custom("expected 32 bytes"))?;
                Ok(NodeKey(arr))
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<NodeKey, E> {
                let arr: [u8; 32] = v.try_into().map_err(|_| E::custom("expected 32 bytes"))?;
                Ok(NodeKey(arr))
            }
        }

        deserializer.deserialize_any(NodeKeyVisitor)
    }
}

impl fmt::Display for NodeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.id(), f)
    }
}

impl fmt::Debug for NodeKey {
    #[inline(always)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.id(), f)
    }
}
