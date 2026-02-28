//! Client to Relay Proto

use std::io;
use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;
use serde_bytes;
use tokio::io::AsyncWriteExt;

use crate::proto::Sender;
use crate::proto::pack::Packable;
use crate::proto::pack::Packer;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum HandshakeP {
    ClientHello {
        /// Identity Public Key (Ed25519)
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
    },

    ServerChallenge {
        /// Random, single-use nonce
        #[serde(with = "serde_bytes")]
        nonce: [u8; 32],
    },

    ClientProof {
        /// Ed25519 signature over:
        /// hash("relay-auth-v1" || nonce)
        #[serde(with = "serde_bytes")]
        sig: [u8; 64],
    },

    ServerAccept {
        /// System time in seconds
        timestamp: u64,
    },

    ServerReject {
        reason: String,
    },
}

/// Miscellaneous Packets
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum MiscP {
    PubAddressReq,
    PubAddressRes { addr: SocketAddr },
}

/// Message forwarding through the relay
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct ForwardP {
    #[serde(with = "serde_bytes")]
    pub to:      [u8; 32],
    #[serde(with = "serde_bytes")]
    pub from:    [u8; 32],
    #[serde(with = "serde_bytes")]
    pub payload: Vec<u8>,
    /// Ed25519 signature over (to ‖ from ‖ payload)
    #[serde(with = "serde_bytes")]
    pub sig:     [u8; 64],
}

/// Relay's response to a Forward request
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum ForwardResult {
    /// Message accepted for delivery
    Accepted,
    /// Recipient not found in DHT
    NotFound,
    /// Signature verification failed
    InvalidSig,
    /// Generic error
    Error { reason: String },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum RelayPacket {
    Handshake(HandshakeP),
    Misc(MiscP),
    /// Client sends a message to be forwarded to another user
    Forward(ForwardP),
    /// Relay responds to a Forward request
    ForwardResult(ForwardResult),
    /// Relay delivers a message to the connected recipient
    Deliver(ForwardP),
}

impl Packable for RelayPacket {}

impl Sender for RelayPacket {
    async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin + Send)) -> Result<(), io::Error> {
        let packet = self.pack().map_err(io::Error::other)?;

        tx.write_all(&packet).await?;
        tx.flush().await
    }
}
