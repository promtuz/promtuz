//! cli - client
//! rel - relay
//! res - resolver

use std::io;

use tokio::io::AsyncWriteExt;

use crate::proto::pack::Packer;
use crate::quic::id::NodeId;

pub mod client_peer;
pub mod client_rel;
pub mod client_res;
pub mod pack;
pub mod peer;
pub mod relay_peer;
#[cfg(feature = "server")]
pub mod relay_res;

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
