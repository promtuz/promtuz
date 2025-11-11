pub static PROTOCOL_VERSION: u16 = 1;

#[cfg(feature = "crypto")]
pub mod crypto;

/// contains serializable message structure for communication between relay <-> resolver <- client
#[cfg(feature = "msg")]
pub mod msg;

#[cfg(feature = "quic")]
pub mod quic;
