//! Contains structs for communication b/w client and relay

use bincode::Decode;
use bincode::Encode;
use serde::Deserialize;
use serde::Serialize;

/// Packet headers for handshake events
///
/// Every handshake event must start with one of these events
#[derive(Serialize, Deserialize, Encode, Decode, Debug, PartialEq, Eq)]
pub enum HandshakePacket {
    ClientHello {
        /// Identity Public Key
        #[serde(with = "serde_bytes")]
        ipk: [u8; 32],
        /// Client's Ephemeral Public Key
        #[serde(with = "serde_bytes")]
        epk: [u8; 32],
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

// impl HandshakePacket {
//     pub fn to_bytes(&self) -> Vec<u8> {
//         let mut enc = bincode::encode_to_vec(self, bincode::config::standard()).unwrap();
//         if !enc.is_empty() {
//             enc[0] += 1;
//         }
//         enc
//     }

//     pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
//         let mut buf = Vec::from(bytes);
//         if !buf.is_empty() {
//             buf[0] -= 1;
//         }
//         println!("GONNA DECODE : {buf:?}");
//         println!("GONNA DECODE : {bytes:?}");
//         match bincode::borrow_decode_from_slice(&buf, bincode::config::standard()) {
//             Ok(shi) => {
//                 println!("SHII : {shi:?}");
//                 shi.0
//             },
//             Err(err) => {
//                 println!("Decode Error : {err}");

//                 None
//             },
//         }
//     }
// }

// #[cfg(test)]
// mod tests {
//     use crate::proto::client::HandshakePacket;

//     #[test]
//     fn test_handshake() {
//         let stat = HandshakePacket::ServerChallenge { epk: [0; 32], ct: [255; 32] };

//         let server_hello_buf = stat.to_bytes();
//         let server_hello = HandshakePacket::from_bytes(&server_hello_buf);

//         assert_eq!(
//             server_hello,
//             Some(HandshakePacket::ServerChallenge { epk: [0; 32], ct: [255; 32] })
//         );
//     }
// }
