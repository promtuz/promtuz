use std::{fmt, ops::Deref, str::FromStr};

use data_encoding::BASE32_NOPAD;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
        let decoded = BASE32_NOPAD
            .decode(s.as_bytes())
            .map_err(|_| "bad base32")?;
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

pub type NodeId = BaseId<10>;

pub fn derive_node_id(pubkey: &p256::PublicKey) -> NodeId {
    let encoded = pubkey.to_encoded_point(false);
    let hash = blake3::hash(encoded.as_bytes());
    NodeId::from_bytes(hash.as_bytes()[..10].try_into().unwrap())
}

pub type UserId = BaseId<12>;

impl UserId {
    pub fn derive(seed: &[u8; 32]) -> Self {
        derive_user_id(seed)
    }
}

pub fn derive_user_id(seed: &[u8; 32]) -> UserId {
    let hash = blake3::hash(seed);
    UserId::from_bytes(hash.as_bytes()[..12].try_into().unwrap())
}
