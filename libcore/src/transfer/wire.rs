//! Chunked-manifest types and the stream frame codec for P2P transfers.

use anyhow::Result;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: usize = 256 * 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Manifest {
    pub total_size: u64,
    pub chunk_size: u32,
    pub chunks: Vec<[u8; 32]>,
}

impl Manifest {
    /// Streams `path` a chunk at a time, hashing each block, so a multi-GB file
    /// never lands in memory whole.
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

    /// Content-addresses the manifest itself, not the file bytes, so two
    /// manifests that describe the same chunks always resolve to the same id.
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
    // serde has no built-in impl for arrays past 32 (see common::types::bytes::Bytes<N>
    // for the same fix elsewhere in the tree).
    #[serde(with = "serde_bytes")]
    pub sig: [u8; 64],
}

/// Max size of a single length-prefixed frame. Bounds the reader's allocation
/// and stops an oversize write from silently truncating under the `u32` prefix
/// (a manifest this large already describes a multi-TB file).
const MAX_FRAME: usize = 8 * 1024 * 1024;

pub async fn write_frame<T: Serialize>(w: &mut quinn::SendStream, v: &T) -> Result<()> {
    let bytes = postcard::to_allocvec(v)?;
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(n <= MAX_FRAME, "frame too large");
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_file_id_is_deterministic() {
        let m1 = Manifest {
            total_size: 10,
            chunk_size: 4,
            chunks: vec![[1u8; 32], [2u8; 32], [3u8; 32]],
        };
        let m2 = m1.clone();
        assert_eq!(m1.file_id(), m2.file_id());

        let mut m3 = m1.clone();
        m3.chunks[0] = [9u8; 32];
        assert_ne!(m1.file_id(), m3.file_id());
    }

    #[test]
    fn from_file_chunks_a_partial_trailing_block() {
        let path = std::env::temp_dir().join("promtuz-from_file-300k.bin");
        std::fs::write(&path, vec![0xabu8; 300 * 1024]).unwrap();

        let m = Manifest::from_file(path.to_str().unwrap()).unwrap();

        assert_eq!(m.total_size, 300 * 1024);
        assert_eq!(m.chunk_size, CHUNK_SIZE as u32);
        assert_eq!(m.chunks.len(), 2); // 256KB + 44KB
    }

    #[test]
    fn from_file_exact_multiple_has_no_empty_trailing_chunk() {
        let path = std::env::temp_dir().join("promtuz-from_file-exact.bin");
        std::fs::write(&path, vec![0u8; 2 * CHUNK_SIZE]).unwrap();

        let m = Manifest::from_file(path.to_str().unwrap()).unwrap();

        assert_eq!(m.total_size, 2 * CHUNK_SIZE as u64);
        assert_eq!(m.chunks.len(), 2);
    }

    #[test]
    fn frame_types_roundtrip_through_postcard() {
        let pull = Pull { file_id: [7u8; 32], have: 3 };
        let got: Pull = postcard::from_bytes(&postcard::to_allocvec(&pull).unwrap()).unwrap();
        assert_eq!(pull, got);

        let manifest = Manifest {
            total_size: 1,
            chunk_size: 1,
            chunks: vec![[1u8; 32]],
        };
        let resp = ServeResp::Manifest(manifest);
        let got: ServeResp = postcard::from_bytes(&postcard::to_allocvec(&resp).unwrap()).unwrap();
        assert_eq!(resp, got);

        let gone = ServeResp::Gone;
        let got: ServeResp = postcard::from_bytes(&postcard::to_allocvec(&gone).unwrap()).unwrap();
        assert_eq!(gone, got);

        let auth = Auth { ipk: [1u8; 32], tls_pub: [2u8; 32], sig: [3u8; 64] };
        let got: Auth = postcard::from_bytes(&postcard::to_allocvec(&auth).unwrap()).unwrap();
        assert_eq!(auth, got);
    }
}
