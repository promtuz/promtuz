use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

use anyhow::Result;
use data_encoding::BASE32_NOPAD;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Visitor;

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: BASE ID :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

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

impl<const N: usize> Deref for BaseId<N> {
    type Target = str;

    fn deref(&self) -> &str {
        str::from_utf8(self.as_bytes()).unwrap()
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

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: NODE ID :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

pub type NodeId = BaseId<10>;

impl NodeId {
    /// Generate new NodeId from public key
    pub fn new<K: AsRef<[u8]>>(key: K) -> Self {
        let hash = blake3::hash(key.as_ref());
        Self::from_bytes(hash.as_bytes()[..Self::LEN].try_into().unwrap())
    }
}

//====:====:====:====: NODE KEY :===:====:====:====||

/// Node's ED25519 Public Key
#[derive(Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeKey([u8; 32]);

impl NodeKey {
    pub const LEN: usize = 32;

    /// Create new NodeKey from Node's Public Key
    pub fn new<K: AsRef<[u8]>>(key: K) -> Result<Self> {
        let key = key.as_ref();
        Ok(Self(key.try_into()?))
    }

    /// Derives a human readable id from public key
    ///
    /// Reverse is not possible
    pub fn id(&self) -> NodeId {
        self.derive_id()
    }

    pub fn key(&self) -> String {
        hex::encode_upper(self.0)
    }

    #[inline]
    fn derive_id(&self) -> BaseId<{ NodeId::LEN }> {
        let hash = blake3::hash(&self.0);
        BaseId::from_bytes(hash.as_bytes()[..NodeId::LEN].try_into().unwrap())
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
                // parse hex string, for example
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

//====:====:====: FORWARDING TRAITS :====:====:====||

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

//====:====:====:===================:====:====:====||

// // // // // // // // // // // // // // // // // //

//===:===:===:===:===:===:=:===:===:===:===:===:===||
//===:===:===:===:===: USER ID :===:===:===:===:===||
//===:===:===:===:===:===:=:===:===:===:===:===:===||

pub type UserId = BaseId<12>;

impl UserId {
    pub fn derive(ed25519_pubkey: &[u8; 32]) -> Self {
        derive_user_id(ed25519_pubkey)
    }
}

pub fn derive_user_id(seed: &[u8; 32]) -> UserId {
    let hash = blake3::hash(seed);
    UserId::from_bytes(hash.as_bytes()[..12].try_into().unwrap())
}
