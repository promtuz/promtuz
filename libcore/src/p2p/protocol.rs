//! The attachment ALPN, independent of relay and MLS versions. QUIC requires an agreed protocol, so
//! a peer that does not offer it fails the handshake.

// Frozen: deriving it from PROTOCOL_VERSION would break attachment compatibility on an
// unrelated version bump.
const ALPN: &[u8] = b"promtuz-attachment/2";

pub(crate) fn offered_alpns() -> Vec<Vec<u8>> {
    vec![ALPN.to_vec()]
}
