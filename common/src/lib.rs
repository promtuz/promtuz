pub static PROTOCOL_VERSION: u16 = 1;

#[cfg(feature = "crypto")]
pub mod crypto;

/// contains serializable message structure for communication between relay <-> resolver <- client
#[cfg(feature = "proto")]
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
#[macro_use]
pub mod server;
