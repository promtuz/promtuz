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
///
/// Identity-key separation: every packet that establishes a contact carries
/// an `ipk` + `ipk_sig` pair that proves the sender's long-term IPK signs
/// off on the TLS sub-key embedded in their cert SPKI. Receivers verify
/// (`verify_ipk_binding`) and only then save the contact under `ipk`. The
/// TLS sub-key is *not* stored as a contact identifier; it is only used for
/// the QUIC handshake.
#[derive(Serialize, Deserialize, Debug)]
pub enum IdentityP {
    AddMe {
        #[serde(with = "serde_bytes")]
        epk:     [u8; 32],
        name:    String,
        /// Long-term IPK of the sender (NOT the cert SPKI).
        #[serde(with = "serde_bytes")]
        ipk:     [u8; 32],
        /// Ed25519 signature by `ipk` over the canonical
        /// `ipk_binding_message(tls_subkey_pubkey)` transcript. The
        /// receiver must verify this against the TLS sub-key it observed
        /// in the peer's cert before treating `ipk` as authentic.
        #[serde(with = "serde_bytes")]
        ipk_sig: [u8; 64],
    },
    /// Cancels the [`IdentityP::AddMe`] request
    NeverMind {},

    /// Rejection for [`IdentityP::AddMe`]
    No { reason: String },
    /// Proceeding with [`IdentityP::AddMe`]
    AddedYou {
        /// Ephemeral key of sharer as it was not included in the IdentityQr
        #[serde(with = "serde_bytes")]
        epk:     [u8; 32],
        /// Long-term IPK of the sharer.
        #[serde(with = "serde_bytes")]
        ipk:     [u8; 32],
        /// Ed25519 signature by `ipk` over `ipk_binding_message(tls_subkey)`.
        #[serde(with = "serde_bytes")]
        ipk_sig: [u8; 64],
    },
    /// Scanner confirms it saved the contact, sharer can now save too
    Confirmed,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ClientPeerPacket {
    Identity(IdentityP),
}

impl Sender for ClientPeerPacket {}
