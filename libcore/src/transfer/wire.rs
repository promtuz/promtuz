//! Chunked-manifest types and the stream frame codec for P2P transfers.

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: usize = 256 * 1024;
// Auth encodes to 129 bytes and Pull to at most 37. These small limits stop parallel
// unauthenticated streams from allocating a manifest-sized buffer before validation.
pub(crate) const AUTH_FRAME_LIMIT: usize = 256;
pub(crate) const PULL_FRAME_LIMIT: usize = 64;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Manifest {
    pub total_size: u64,
    pub chunk_size: u32,
    pub chunks: Vec<[u8; 32]>,
}

impl Manifest {
    pub fn from_file(path: &str) -> Result<Manifest> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut chunks = Vec::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; CHUNK_SIZE];
        loop {
            let mut filled = 0;
            while filled < CHUNK_SIZE {
                let n = f.read(&mut buf[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            chunks.push(*blake3::hash(&buf[..filled]).as_bytes());
            total += filled as u64;
            if filled < CHUNK_SIZE {
                break;
            }
        }
        Ok(Manifest { total_size: total, chunk_size: CHUNK_SIZE as u32, chunks })
    }

    /// Content-addresses the manifest, not the file bytes: the same chunks always give the same id.
    pub fn file_id(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"promtuz/transfer/manifest");
        h.update(&self.total_size.to_le_bytes());
        h.update(&self.chunk_size.to_le_bytes());
        for c in &self.chunks {
            h.update(c);
        }
        *h.finalize().as_bytes()
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub struct Pull {
    pub file_id: [u8; 32],
    pub have: u32,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
pub enum ServeResp {
    Manifest(Manifest),
    Gone,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Auth {
    pub ipk: [u8; 32],
    pub tls_pub: [u8; 32],
    #[serde(with = "serde_bytes")]
    pub sig: [u8; 64],
}

/// Bounds the reader's allocation and keeps a write from truncating under the `u32` length
/// prefix; a manifest this large already describes a multi-TB file.
const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("invalid transfer frame: {0}")]
pub(crate) struct InvalidFrame(pub(crate) String);

pub async fn write_frame<T: Serialize>(w: &mut quinn::SendStream, v: &T) -> Result<()> {
    let bytes = postcard::to_allocvec(v)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> Result<T> {
    read_frame_limited(r, MAX_FRAME).await
}

pub(crate) async fn read_frame_limited<T: DeserializeOwned>(
    r: &mut quinn::RecvStream, limit: usize,
) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as usize;
    if n > limit.min(MAX_FRAME) {
        return Err(InvalidFrame("frame too large".into()).into());
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    postcard::from_bytes(&buf).map_err(|e| InvalidFrame(e.to_string()).into())
}
