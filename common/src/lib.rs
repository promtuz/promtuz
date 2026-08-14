/// Client ↔ relay protocol version, mixed into the relay-auth handshake and
/// every relay-verified signing transcript.
///
/// A bump is a flag day for both sides at once: the relay verifies transcripts
/// against its own copy, so a client on either side of a change is refused. Move
/// it only alongside a relay deploy, and prefer moving it to letting a shape
/// change fail as an unexplained signature error.
///
/// 6: `ActivityP` carries the conversation it happened in.
pub static PROTOCOL_VERSION: u16 = 6;

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
pub mod server;

#[cfg(feature = "types")]
pub mod types;
