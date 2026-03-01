use std::ops::Deref;
use std::ops::DerefMut;

use serde::Deserialize;
use serde::Serialize;

#[derive(Serialize, Deserialize, Debug, Copy, PartialEq, Eq, Clone)]
#[serde(transparent)]
pub struct Bytes<const N: usize>(#[serde(with = "serde_bytes")] pub [u8; N]);

impl<const N: usize> Deref for Bytes<N> {
    type Target = [u8; N];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<const N: usize> DerefMut for Bytes<N> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<const N: usize> AsRef<[u8]> for Bytes<N> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> From<[u8; N]> for Bytes<N> {
    fn from(b: [u8; N]) -> Self {
        Self(b)
    }
}

impl<const N: usize> From<Bytes<N>> for [u8; N] {
    fn from(b: Bytes<N>) -> Self {
        b.0
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
#[serde(transparent)]
pub struct ByteVec(#[serde(with = "serde_bytes")] pub Vec<u8>);

impl Deref for ByteVec {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ByteVec {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsRef<[u8]> for ByteVec {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for ByteVec {
    fn from(v: Vec<u8>) -> Self {
        Self(v)
    }
}

impl From<ByteVec> for Vec<u8> {
    fn from(b: ByteVec) -> Self {
        b.0
    }
}
