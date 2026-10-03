/// Version in the `role/N` ALPNs and in the handshake and signing transcripts nodes verify. A bump
/// is a flag day for every node and client at once.
pub const PROTOCOL_VERSION: u16 = 11;

/// Messages queued under the previous version still verify.
pub const SIGNATURE_VERSIONS: [u16; 2] = [PROTOCOL_VERSION, 10];

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
