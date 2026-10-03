//! CA-attested node capabilities: bits the RootCA stamps into a leaf cert extension, so verifying
//! the chain verifies them and a node cannot self-assert one.

use bitflags::bitflags;

/// Unregistered private arc; it only has to be unique inside our own closed PKI.
pub const CAPABILITY_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 58888, 1];

bitflags! {
    /// Add bits, never renumber.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct NodeCapabilities: u32 {
        const RELAY             = 1 << 0; // basic store-and-forward (every relay)
        const PUSH_GATEWAY      = 1 << 1; // holds APNs/FCM creds, runs the wake path
        const BLOB_STORE        = 1 << 2; // content-addressed encrypted media
        const CALL_RELAY        = 1 << 3; // SFrame / TURN for A/V
        const HIGH_AVAILABILITY = 1 << 4; // tier-1 stable-node SLA
        const STICKER_STORE     = 1 << 5; // accepts sticker-pack uploads into the project bucket
    }
}

impl NodeCapabilities {
    pub fn encode(self) -> Vec<u8> {
        self.bits().to_le_bytes().to_vec()
    }

    /// Exactly 4 bytes. Unknown bits are retained, so a newer CA can add a capability.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(|b| Self::from_bits_retain(u32::from_le_bytes(b)))
    }
}
