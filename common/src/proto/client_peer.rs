//! Client to Client (P2P) Proto

use serde::Deserialize;
use serde::Serialize;

use crate::proto::Sender;

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

impl Sender for ClientPeerPacket {}
