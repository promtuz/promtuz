//! CA-attested node capabilities.
//!
//! A capability is a bit the RootCA stamps into a leaf cert's custom X.509
//! extension. Because it rides *inside* the CA-signed cert, anyone who
//! verifies the chain also verifies the capability — a node cannot
//! self-assert one, and there is no separate registry to forge against.
//!
//! This module owns the *semantic* — the bitset, the OID, and the
//! bytes↔bitset codec. The DER plumbing to embed or extract the extension
//! lives with whoever already parses certs: `certgen` writes it,
//! relay/gateway read it off an already-parsed `X509Certificate`.

use bitflags::bitflags;

/// OID arcs for the capability extension. Self-issued private tag: it only has
/// to be unique inside our own closed CA (we are the sole issuer *and* the sole
/// verifier), so it is registered nowhere.
///
// ponytail: arbitrary private arc — swap freely, it's one const and never
// leaves our PKI.
pub const CAPABILITY_OID: &[u64] = &[1, 3, 6, 1, 4, 1, 58888, 1];

bitflags! {
    /// CA-attested capability bitset carried in a node's leaf cert.
    ///
    /// Extensible: add bits, never renumber. Trust (CA tier) and capability are
    /// orthogonal — the tier says *how trusted*, these bits say *what it offers*.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct NodeCapabilities: u32 {
        const RELAY             = 1 << 0; // basic store-and-forward (every relay)
        const PUSH_GATEWAY      = 1 << 1; // holds APNs/FCM creds, runs the wake path
        const BLOB_STORE        = 1 << 2; // content-addressed encrypted media
        const CALL_RELAY        = 1 << 3; // SFrame / TURN for A/V
        const HIGH_AVAILABILITY = 1 << 4; // tier-1 stable-node SLA
    }
}

impl NodeCapabilities {
    /// Bytes to embed as the extension content.
    ///
    // ponytail: plain 4-byte LE u32. If caps ever outgrow 32 bits, widen the
    // backing int here + in `decode` and bump the format.
    pub fn encode(self) -> Vec<u8> {
        self.bits().to_le_bytes().to_vec()
    }

    /// Parse the extension content back to a bitset. Strict length (exactly 4
    /// bytes, else `None`), but `from_bits_retain` keeps bits this build does
    /// not know — a newer CA can add a capability without an older node
    /// mangling it.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(|b| Self::from_bits_retain(u32::from_le_bytes(b)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let caps = NodeCapabilities::PUSH_GATEWAY | NodeCapabilities::HIGH_AVAILABILITY;
        assert_eq!(NodeCapabilities::decode(&caps.encode()), Some(caps));
    }

    #[test]
    fn contains_checks_bits() {
        let caps = NodeCapabilities::PUSH_GATEWAY;
        assert!(caps.contains(NodeCapabilities::PUSH_GATEWAY));
        assert!(!caps.contains(NodeCapabilities::BLOB_STORE));
    }

    #[test]
    fn decode_retains_unknown_future_bits() {
        // bit 31 isn't defined here; a newer CA might set it. We must not drop it.
        let raw = (1u32 << 31).to_le_bytes();
        assert_eq!(NodeCapabilities::decode(&raw).map(|c| c.bits()), Some(1 << 31));
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert_eq!(NodeCapabilities::decode(&[]), None);
        assert_eq!(NodeCapabilities::decode(&[1, 2, 3]), None);
    }
}
