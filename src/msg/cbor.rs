use std::io::Cursor;

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub trait ToCbor {
    fn to_cbor(&self) -> Result<Vec<u8>>;
    fn pack(&self) ->  Result<Vec<u8>>;
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

    /// Frames bytes after CBOR Encoding as ready to transmit Packet
    fn pack(&self) ->  Result<Vec<u8>> {
        let packet = self.to_cbor()?;
        let size: [u8; 4] = (packet.len() as u32).to_be_bytes();
        Ok([&size, packet.as_slice()].concat())
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
