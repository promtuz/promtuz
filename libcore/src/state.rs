//! Process-global state singletons.
//!
//! Extracted from `quic::server` so consumers that need the global
//! `RELAY` (e.g. `messaging::sendMessage`, which reads it for the
//! per-connection `RelayDhtClient` dialer) can do so without forming
//! an intra-crate cycle:
//!
//! ```text
//!   messaging  ─┐
//!                    ├─→  state::RELAY
//!   quic::server   ──┘
//! ```
//!
//! Previously both modules referred to each other through the global
//! sitting in `quic::server`, which the `cycle-detector` flagged as a
//! load-bearing intra-crate cycle. Moving the global to a leaf module
//! breaks the cycle without changing the runtime behaviour: `quic::server`
//! still owns the `Relay` value's lifetime; `state::RELAY` is just the
//! shared box.

use std::sync::atomic::AtomicI32;
use std::sync::atomic::AtomicU64;

use parking_lot::RwLock;

use crate::data::relay::Relay;
use crate::events::connection::ConnectionState;

/// Process-global handle to the connected sticky-home `Relay`.
///
/// Set by `quic::server::Relay::connect` after the `relay/1` handshake
/// succeeds; cleared on disconnect/reconnect. Read by
/// `messaging::sendMessage` (and the receive path) to obtain the
/// per-connection [`crate::quic::relay_dht_client::RelayDhtClient`]
/// dialer for MLS DHT-RPC wrappers.
pub static RELAY: RwLock<Option<Relay>> = RwLock::new(None);

/// Last-known connection state, mirroring the typed `ConnectionState`
/// event. Backed by an atomic so a synchronous "what's the state?" read
/// doesn't need the event channel. Written by `ConnectionState::emit`.
pub static CONNECTION_STATE: AtomicI32 = AtomicI32::new(ConnectionState::Idle as i32);

/// Wall-clock seconds when the current connection was established.
/// Not reset on disconnect — it's the start time of the *last* connection.
pub static CONNECTION_START_TIME: AtomicU64 = AtomicU64::new(0);
