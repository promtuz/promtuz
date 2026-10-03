use std::fmt;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::SeqAccess;
use serde::de::Visitor;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use tokio::time::timeout_at;

/// Frame = 4-byte big-endian length + body. A larger length fails before the body is read.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Bounds a frame once started. Waiting for the first byte of the next frame stays unbounded,
/// since an idle persistent stream is legitimate.
pub const FRAME_READ_TIMEOUT: Duration = Duration::from_secs(20);

const FRAME_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum PackError {
    #[error("failed to serialize: {0}")]
    SerFailed(postcard::Error),
    #[error("packet too large: {0} bytes exceeds MAX_FRAME_BYTES")]
    FrameTooLarge(usize),
}

#[derive(Debug, Error)]
pub enum UnpackError {
    #[error("failed to read: {0}")]
    ReadFailed(io::Error),
    #[error("failed to deserialize: {0}")]
    DeserFailed(postcard::Error),
    #[error("frame too large: {0} bytes exceeds MAX_FRAME_BYTES")]
    FrameTooLarge(usize),
    #[error("frame stalled for more than {}s", FRAME_READ_TIMEOUT.as_secs())]
    ReadTimedOut,
    #[error("trailing bytes after framed packet")]
    TrailingBytes,
}

pub trait Packer {
    fn ser(&self) -> Result<Vec<u8>, PackError>;
    fn pack(&self) -> Result<Vec<u8>, PackError>;
}

impl<T> Packer for T
where
    T: Serialize,
{
    #[inline]
    fn ser(&self) -> Result<Vec<u8>, PackError> {
        postcard::to_allocvec(self).map_err(PackError::SerFailed)
    }

    #[inline]
    fn pack(&self) -> Result<Vec<u8>, PackError> {
        let packet = self.ser()?;
        if packet.len() > MAX_FRAME_BYTES {
            return Err(PackError::FrameTooLarge(packet.len()));
        }
        let len = packet.len() as u32;
        let mut out = Vec::with_capacity(4 + packet.len());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&packet);
        Ok(out)
    }
}

pub trait Unpacker: Sized {
    fn deser(bytes: &[u8]) -> Result<Self, UnpackError>;

    fn unpack<R>(rx: &mut R) -> impl Future<Output = Result<Self, UnpackError>> + Send
    where
        R: AsyncReadExt + Unpin + Send;
}

impl<T> Unpacker for T
where
    T: DeserializeOwned + Send,
{
    #[inline]
    fn deser(bytes: &[u8]) -> Result<Self, UnpackError> {
        let (value, tail) = postcard::take_from_bytes(bytes).map_err(UnpackError::DeserFailed)?;
        if !tail.is_empty() {
            return Err(UnpackError::TrailingBytes);
        }
        Ok(value)
    }

    fn unpack<R>(rx: &mut R) -> impl Future<Output = Result<Self, UnpackError>> + Send
    where
        R: AsyncReadExt + Unpin + Send,
    {
        unpack(rx)
    }
}

async fn read_exact_by<R: AsyncReadExt + Unpin + Send>(
    rx: &mut R, buf: &mut [u8], deadline: Instant,
) -> Result<(), UnpackError> {
    timeout_at(deadline, rx.read_exact(buf))
        .await
        .map_err(|_| UnpackError::ReadTimedOut)?
        .map_err(UnpackError::ReadFailed)?;
    Ok(())
}

#[inline(always)]
pub async fn unpack<T: Unpacker, R: AsyncReadExt + Unpin + Send>(
    rx: &mut R,
) -> Result<T, UnpackError> {
    unpack_optional(rx).await?.ok_or_else(|| {
        UnpackError::ReadFailed(io::Error::new(io::ErrorKind::UnexpectedEof, "missing packet"))
    })
}

/// `None` is a clean end between frames. A partial header or body is an error, so a queue drain
/// cannot acknowledge a truncated batch.
pub async fn unpack_optional<T: Unpacker, R: AsyncReadExt + Unpin + Send>(
    rx: &mut R,
) -> Result<Option<T>, UnpackError> {
    let mut len = [0u8; 4];
    if rx.read(&mut len[..1]).await.map_err(UnpackError::ReadFailed)? == 0 {
        return Ok(None);
    }
    let deadline = Instant::now() + FRAME_READ_TIMEOUT;
    read_exact_by(rx, &mut len[1..], deadline).await?;

    let frame_size = u32::from_be_bytes(len) as usize;
    if frame_size > MAX_FRAME_BYTES {
        return Err(UnpackError::FrameTooLarge(frame_size));
    }

    // One deadline for the whole body: per-chunk deadlines would let a trickling peer hold the
    // buffer for FRAME_READ_TIMEOUT times the chunk count.
    let mut frame = Vec::new();
    while frame.len() < frame_size {
        let at = frame.len();
        let chunk = (frame_size - at).min(FRAME_CHUNK_BYTES);
        frame.resize(at + chunk, 0);
        read_exact_by(rx, &mut frame[at..], deadline).await?;
    }

    T::deser(&frame).map(Some)
}

/// Refuses more than `MAX` elements while reading, so a peer cannot force a larger allocation.
pub fn bounded_vec<'de, D, T, const MAX: usize>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    d.deserialize_seq(BoundedVec::<T, MAX>(PhantomData))
}

struct BoundedVec<T, const MAX: usize>(PhantomData<T>);

impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for BoundedVec<T, MAX> {
    type Value = Vec<T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a sequence of at most {MAX} elements")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(MAX));
        while let Some(item) = seq.next_element()? {
            if out.len() == MAX {
                return Err(serde::de::Error::invalid_length(MAX + 1, &self));
            }
            out.push(item);
        }
        Ok(out)
    }
}
