//! Wire types. In module names, rel is relay and res is resolver.

use std::io;

use tokio::io::AsyncWriteExt;

use crate::proto::pack::Packer;
use crate::types::id::NodeId;

pub mod client_rel;
pub mod client_res;
pub mod dht_p2p;
pub mod mls_wire;
pub mod p2p_relay;
pub mod pack;
pub mod push;
pub mod profile;
#[cfg(all(feature = "server", feature = "proto"))]
pub mod relay_res;
pub mod sticker;

pub type RelayId = NodeId;
pub type ResolverId = NodeId;

pub trait Sender: Packer {
    fn send(
        &self, tx: &mut (impl AsyncWriteExt + Unpin + Send),
    ) -> impl std::future::Future<Output = Result<(), std::io::Error>> + Send
    where
        Self: std::marker::Sync,
    {
        async {
            let packet = self.pack().map_err(io::Error::other)?;
            tx.write_all(&packet).await?;
            tx.flush().await
        }
    }
}

/// Each transcript's BLAKE3 digest must equal the digest at its position in `want`.
#[cfg(test)]
pub(crate) fn golden(transcripts: &[Vec<u8>], want: &str) {
    let want: Vec<&str> = want.split_whitespace().collect();
    assert_eq!(transcripts.len(), want.len());
    for (i, (t, want)) in transcripts.iter().zip(want).enumerate() {
        assert_eq!(blake3::hash(t).to_hex().as_str(), want, "transcript {i}: {}", hex::encode(t));
    }
}
