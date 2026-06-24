//! Process-global state singletons.
//!
//! **Phase 8 (P1 #19)**: extracted from `quic::server` so consumers
//! that need the global `RELAY` (e.g. `api::messaging::sendMessage`,
//! which reads it for the per-connection `RelayDhtClient` dialer)
//! can do so without forming an intra-crate cycle:
//!
//! ```text
//!   api::messaging  ─┐
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

use parking_lot::RwLock;

use crate::data::relay::Relay;

/// Process-global handle to the connected sticky-home `Relay`.
///
/// Set by `quic::server::Relay::connect` after the `relay/1` handshake
/// succeeds; cleared on disconnect/reconnect. Read by
/// `api::messaging::sendMessage` (and the receive path) to obtain the
/// per-connection [`crate::quic::relay_dht_client::RelayDhtClient`]
/// dialer for MLS DHT-RPC wrappers.
pub static RELAY: RwLock<Option<Relay>> = RwLock::new(None);
