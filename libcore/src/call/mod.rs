//! Calls. Signaling arrives here off the MLS channel; the engine that acts
//! on it lands with the audio path.

use common::proto::mls_wire::CallMsg;

/// Life of every call signal at the relay. An offer is worth nothing after
/// the ring, and the rest of a call is worth nothing after the offer.
pub(crate) const SIGNAL_TTL_MS: u64 = 40_000;

pub(crate) fn on_signal(from: [u8; 32], _conversation: [u8; 16], signal: CallMsg) {
    log::info!("CALL[{}]: {signal:?} dropped, no engine in this build", hex::encode(&from[..4]));
}
