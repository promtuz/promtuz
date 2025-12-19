use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub enum HandshakePacket {
    ClientHello {
        /// Identity Public Key
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
        // /// Client's Ephemeral Public Key
        // #[serde(with = "serde_bytes")]
        // epk: [u8; 32],
    },

    ServerChallenge {
        /// Server's Ephemeral Public Key
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
        /// Encrypted Payload that client shall decrypt and send
        #[serde(with = "serde_bytes")]
        ct: [u8; 32],
    },

    ClientProof {
        /// Decrypted ServerChallenge#msg
        #[serde(with = "serde_bytes")]
        proof: [u8; 16],
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
    PubAddressReq {
        // will response append `:<port>` in addr
        // isn't needed but can't leave struct empty
        port: bool,
    },
    PubAddressRes {
        addr: String,
    },
}
