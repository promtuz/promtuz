//! Client to Client (P2P) Proto

use std::io;

use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::proto::Sender;
use crate::proto::pack::Packable;
use crate::proto::pack::Packer;

/// Packets for exchanging identity over peer to peer network
///
/// Exchange Flow:
/// 1. [`libcore:..:IdentityQr`] is shared.
/// 2. Scanner will connect to sharer and send [`IdentityP::AddMe`]
/// 3. Sharer can either send [`IdentityP::No`] or [`IdentityP::AddedYou`]
/// 4. Scanner saves contact and sends [`IdentityP::Confirmed`]
/// 5. Sharer saves contact only after receiving [`IdentityP::Confirmed`]
#[derive(Serialize, Deserialize, Debug)]
pub enum IdentityP {
    AddMe {
        #[serde(with = "serde_bytes")]
        epk:  [u8; 32],
        name: String,
    },
    /// Cancels the [`IdentityP::AddMe`] request
    NeverMind {},

    /// Rejection for [`IdentityP::AddMe`]
    No { reason: String },
    /// Proceeding with [`IdentityP::AddMe`]
    AddedYou {
        /// Ephemeral key of sharer as it was not included in the IdentityQr
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
    },
    /// Scanner confirms it saved the contact, sharer can now save too
    Confirmed,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientPeerPacket {
    Identity(IdentityP),
}

impl Packable for ClientPeerPacket {}

impl Sender for ClientPeerPacket {
    async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> Result<(), io::Error> {
        let packet = self.pack().map_err(io::Error::other)?;

        tx.write_all(&packet).await?;
        tx.flush().await
    }
}
