use std::net::{IpAddr};

use serde::{Deserialize, Serialize};

use serde_bytes;

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum HandshakePacket {
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
pub enum MiscPacket {
    PubAddressReq,
    PubAddressRes {
        addr: IpAddr,
    },
}
