use std::io;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::AsyncReadExt;

#[derive(Debug, Error)]
pub enum PackError {
    #[error("failed to serialize: {0}")]
    SerFailed(postcard::Error),
}

#[derive(Debug, Error)]
pub enum UnpackError {
    #[error("failed to read: {0}")]
    ReadFailed(io::Error),
    #[error("failed to deserialize: {0}")]
    DeserFailed(postcard::Error),
}

/// Decides which structs and enums can be packed for network transmission
///
/// Only use for data that is sent over network and not locally
pub trait Packable {}

pub trait Packer {
    fn ser(&self) -> Result<Vec<u8>, PackError>;
    fn pack(&self) -> Result<Vec<u8>, PackError>;
}

impl<T> Packer for T
where
    T: Serialize + Packable,
{
    #[inline]
    fn ser(&self) -> Result<Vec<u8>, PackError> {
        postcard::to_allocvec(self).map_err(PackError::SerFailed)
    }

    /// Frames bytes after serializing as ready to transmit Packet
    #[inline]
    fn pack(&self) -> Result<Vec<u8>, PackError> {
        let packet = self.ser()?;
        let size: [u8; 2] = (packet.len() as u16).to_be_bytes();
        Ok([&size, packet.as_slice()].concat())
    }
}

#[async_trait]
pub trait Unpacker: Sized {
    fn deser(bytes: &[u8]) -> Result<Self, UnpackError>;

    async fn unpack<R>(rx: &mut R) -> Result<Self, UnpackError>
    where
        R: AsyncReadExt + Unpin + Send;
}

#[async_trait]
impl<T> Unpacker for T
where
    T: DeserializeOwned,
{
    #[inline]
    fn deser(bytes: &[u8]) -> Result<Self, UnpackError> {
        // let cursor = Cursor::new(bytes);
        // Ok(ciborium::de::from_reader(cursor)?)

        postcard::from_bytes(bytes).map_err(UnpackError::DeserFailed)
    }

    async fn unpack<R>(rx: &mut R) -> Result<Self, UnpackError>
    where
        R: AsyncReadExt + Unpin + Send,
    {
        unpack(rx).await
    }
}


#[inline(always)]
pub async fn unpack<T: DeserializeOwned, R: AsyncReadExt + Unpin + Send>(
    rx: &mut R,
) -> Result<T, UnpackError> {
    let frame_size = rx.read_u16().await.map_err(UnpackError::ReadFailed)?;
    let mut frame = vec![0u8; frame_size as usize];
    rx.read_exact(&mut frame).await.map_err(UnpackError::ReadFailed)?;

    T::deser(&frame)
}
