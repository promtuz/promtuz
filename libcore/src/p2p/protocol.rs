//! The attachment ALPN, independent of relay and MLS versions. QUIC requires an agreed protocol, so
//! a peer that does not offer it fails the handshake.

// Frozen: deriving it from PROTOCOL_VERSION would break attachment compatibility on an
// unrelated version bump.
pub(crate) const ALPN: &[u8] = b"promtuz-attachment/2";
