use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use serde_bytes;

use crate::msg::cbor::ToCbor;

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
    PubAddressRes { addr: IpAddr },
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum RelayPacket {
    Handshake(HandshakeP),
    Misc(MiscP),
}

impl RelayPacket {
    pub async fn send(self, tx: &mut (impl AsyncWriteExt + Unpin)) -> anyhow::Result<()> {
        let packet = self.pack()?;

        tx.write_all(&packet).await?;
        Ok(tx.flush().await?)
    }
}
