//! Client to Client (P2P) Proto

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::proto::pack::Packable;
use crate::proto::pack::Packer;

/// Packets for exchanging identity over peer to peer network
///
/// Exchange Flow:
/// 1. [`libcore:..:IdentityQr`] is shared.
/// 2. Scanner will connect to sharer and send [`IdentityP::AddMe`]
/// 3. Sharer can either send [`IdentityP::No`] or [`IdentityP::AddedYou`]
/// 4. Scanner will also save epk of sharer if [`IdentityP::AddedYou`]
#[derive(Serialize, Deserialize, Debug)]
pub enum IdentityP {
    AddMe {
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
        name: String,
    },
    /// Rejection for [`IdentityP::AddMe`]
    No { reason: String },

    /// Proceeding with [`IdentityP::AddMe`]
    AddedYou {
        /// Ephemeral key of sharer as it was not included in the IdentityQr
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
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
