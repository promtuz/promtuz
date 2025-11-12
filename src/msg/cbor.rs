use std::io::Cursor;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub trait ToCbor {
    fn to_cbor(&self) -> Result<Vec<u8>>;
}

impl<T> ToCbor for T
where
    T: Serialize,
{
    fn to_cbor(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(self, &mut buf)?;
        Ok(buf)
    }
}

pub trait FromCbor: Sized {
    fn from_cbor(bytes: &[u8]) -> Result<Self>;
}

impl<T> FromCbor for T
where
    T: DeserializeOwned,
{
    fn from_cbor(bytes: &[u8]) -> Result<Self> {
        let cursor = Cursor::new(bytes);
        Ok(ciborium::de::from_reader(cursor)?)
    }
}
