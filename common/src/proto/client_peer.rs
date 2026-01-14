//! Client to Client Proto

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::proto::pack::{Packable, Packer};

/// Packets for exchanging identity over peer to peer network
#[derive(Serialize, Deserialize, Debug)]
pub enum IdentityP {
    AddMe {
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
        name: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientPeerPacket {
    Identity(IdentityP),
}

impl Packable for ClientPeerPacket {}

impl ClientPeerPacket {
    pub async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> anyhow::Result<()> {
        let packet = self.pack()?;

        tx.write_all(&packet).await?;
        Ok(tx.flush().await?)
    }
}
