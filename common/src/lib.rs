/// Version in the `role/N` ALPNs and in the handshake and signing transcripts nodes verify. A bump
/// is a flag day for every node and client at once.
pub const PROTOCOL_VERSION: u16 = 12;

// Remove the shims in common/PROTOCOL_COMPAT.md before retiring this guard.
const _: () = assert!(
    PROTOCOL_VERSION == 12,
    "Protocol changed: remove temporary fallbacks listed in common/PROTOCOL_COMPAT.md"
);

#[cfg(feature = "contracts")]
pub mod contracts;

#[cfg(feature = "crypto")]
pub mod crypto;

#[cfg(any(feature = "proto", feature = "wire"))]
pub mod proto;

#[cfg(feature = "quic")]
pub mod quic;

#[cfg(feature = "sysutils")]
pub mod sysutils;

#[cfg(feature = "macros")]
pub mod macros;

#[cfg(feature = "node")]
pub mod node;

#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "types")]
pub mod types;

pub mod utils;
