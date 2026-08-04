use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::time::Duration;

use async_trait::async_trait;
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

/// Hard cap on one framed packet, checked against the peer-declared length
/// before any body is read. Also the effective max message + inline-media
/// size; larger media rides the out-of-band blob path.
///
/// ponytail: 1 MiB. Raise for bigger inline media, but the relay queues up to
/// MAX_QUEUED_PER_RECIPIENT copies at K homes, so disk scales with this.
pub const MAX_FRAME_BYTES: usize = 1 << 20;

/// Bounds a *stalled* frame. The wait for the first byte of a new frame stays
/// unbounded — an idle persistent stream is legitimate — but once a peer has
/// begun a frame, every subsequent read must complete inside this window.
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
}

/// Decides which structs and enums can be packed for network transmission
///
/// Only use for data that is sent over network and not locally
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

    /// Frames bytes after serializing as ready to transmit Packet
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

async fn read_exact_within<R: AsyncReadExt + Unpin + Send>(
    rx: &mut R, buf: &mut [u8],
) -> Result<(), UnpackError> {
    read_exact_by(rx, buf, Instant::now() + FRAME_READ_TIMEOUT).await
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
pub async fn unpack<T: DeserializeOwned, R: AsyncReadExt + Unpin + Send>(
    rx: &mut R,
) -> Result<T, UnpackError> {
    let mut len = [0u8; 4];
    rx.read_exact(&mut len[..1]).await.map_err(UnpackError::ReadFailed)?;
    read_exact_within(rx, &mut len[1..]).await?;

    let frame_size = u32::from_be_bytes(len) as usize;
    if frame_size > MAX_FRAME_BYTES {
        return Err(UnpackError::FrameTooLarge(frame_size));
    }

    // One deadline for the whole body, not per chunk — otherwise a peer
    // trickling a byte under the limit each interval holds the buffer for
    // FRAME_READ_TIMEOUT × the chunk count.
    let deadline = Instant::now() + FRAME_READ_TIMEOUT;
    let mut frame = Vec::new();
    while frame.len() < frame_size {
        let at = frame.len();
        let chunk = (frame_size - at).min(FRAME_CHUNK_BYTES);
        frame.resize(at + chunk, 0);
        read_exact_by(rx, &mut frame[at..], deadline).await?;
    }

    T::deser(&frame)
}

/// `#[serde(deserialize_with = "bounded_vec::<_, _, LIMIT>")]` — refuses a
/// sequence longer than `MAX` while it is being read, so a peer cannot make
/// the receiver materialise more elements than the protocol permits.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrips_through_u32_prefix() {
        let msg: Vec<u8> = (0..300).map(|i| i as u8).collect(); // > old u8/u16-fiddly sizes
        let framed = msg.pack().expect("pack");
        let body_len = msg.ser().unwrap().len();
        assert_eq!(&framed[..4], &(body_len as u32).to_be_bytes(), "4-byte BE length prefix");
        let mut rx: &[u8] = &framed;
        let out: Vec<u8> = unpack(&mut rx).await.expect("unpack");
        assert_eq!(out, msg);
    }

    #[test]
    fn pack_rejects_oversize() {
        let big = vec![0u8; MAX_FRAME_BYTES + 1];
        assert!(matches!(big.pack(), Err(PackError::FrameTooLarge(_))));
    }

    #[tokio::test]
    async fn unpack_rejects_oversize_length_before_reading_body() {
        // Only the 4-byte length is present (no body) — proves we reject on the
        // declared length before allocating/reading, the OOM guard.
        let framed = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        let mut rx: &[u8] = &framed;
        let r: Result<Vec<u8>, _> = unpack(&mut rx).await;
        assert!(matches!(r, Err(UnpackError::FrameTooLarge(_))));
    }

    #[tokio::test]
    async fn unpack_fails_when_the_declared_body_never_arrives() {
        let mut framed = (MAX_FRAME_BYTES as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&[0u8; 8]);
        let mut rx: &[u8] = &framed;
        let r: Result<Vec<u8>, _> = unpack(&mut rx).await;
        assert!(matches!(r, Err(UnpackError::ReadFailed(_))));
    }

    #[derive(Debug, serde::Deserialize)]
    struct Capped {
        #[serde(deserialize_with = "bounded_vec::<_, _, 3>")]
        items: Vec<u32>,
    }

    #[test]
    fn bounded_vec_accepts_up_to_the_cap() {
        let bytes = vec![1u32, 2, 3].ser().unwrap();
        let out = Capped::deser(&bytes).expect("at cap");
        assert_eq!(out.items, vec![1, 2, 3]);
    }

    #[test]
    fn bounded_vec_rejects_one_past_the_cap() {
        let bytes = vec![1u32, 2, 3, 4].ser().unwrap();
        assert!(matches!(Capped::deser(&bytes), Err(UnpackError::DeserFailed(_))));
    }
}
