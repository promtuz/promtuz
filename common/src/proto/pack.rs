use anyhow::Result;
use anyhow::anyhow;
use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncReadExt;

/// Decides which structs and enums can be packed for network transmission
/// Only for data sent over network, not for local
pub(crate) trait Packable {}

/// FIXME: DO THIS TODO
/// 
/// TODO: Fix naming confusion between cbor and postcard
pub trait Packer {
    fn to_cbor(&self) -> Result<Vec<u8>>;
    fn pack(&self) -> Result<Vec<u8>>;
}

impl<T> Packer for T
where
    T: Serialize + Packable,
{
    #[inline]
    fn to_cbor(&self) -> Result<Vec<u8>> {
        // let mut buf = Vec::new();
        // ciborium::ser::into_writer(self, &mut buf)?;
        // Ok(buf)

        Ok(postcard::to_allocvec(self)?)
    }

    /// Frames bytes after CBOR Encoding as ready to transmit Packet
    #[inline]
    fn pack(&self) -> Result<Vec<u8>> {
        let packet = self.to_cbor()?;
        let size: [u8; 2] = (packet.len() as u16).to_be_bytes();
        Ok([&size, packet.as_slice()].concat())
    }
}

#[async_trait]
pub trait Unpacker: Sized {
    fn from_cbor(bytes: &[u8]) -> Result<Self>;

    async fn unpack<R>(rx: &mut R) -> Result<Self>
    where
        R: AsyncReadExt + Unpin + Send;
}

#[async_trait]
impl<T> Unpacker for T
where
    T: DeserializeOwned,
{
    #[inline]
    fn from_cbor(bytes: &[u8]) -> Result<Self> {
        // let cursor = Cursor::new(bytes);
        // Ok(ciborium::de::from_reader(cursor)?)

        Ok(postcard::from_bytes(bytes)?)
    }

    async fn unpack<R>(rx: &mut R) -> Result<Self>
    where
        R: AsyncReadExt + Unpin + Send,
    {
        unpack(rx).await
    }
}

#[inline(always)]
pub async fn unpack<T: DeserializeOwned, R: AsyncReadExt + Unpin + Send>(rx: &mut R) -> Result<T> {
    let frame_size = rx
        .read_u16()
        .await
        .map_err(|e| anyhow!("can't read packet len: {e}"))?;
    let mut frame = vec![0u8; frame_size as usize];
    rx.read_exact(&mut frame)
        .await
        .map_err(|e| anyhow!("failed to read packet of size {frame_size}: {e}"))?;

    T::from_cbor(&frame)
}
