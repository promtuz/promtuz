//! Contracts the host client implements for core: key custody and event delivery. They live
//! outside `api` because the engine must not depend on the FFI layer.

use crate::events::connection::ConnectionState;
use crate::events::messaging::MessageEv;

/// Platform key store (Android Keystore, iOS Keychain) that seals and opens key material. Only
/// custody of the wrapping key crosses the boundary; crypto stays in core.
#[uniffi::export(with_foreign)]
pub trait SecureStore: Send + Sync {
    fn seal(&self, plaintext: Vec<u8>) -> Result<Vec<u8>, CoreError>;
    fn open(&self, ciphertext: Vec<u8>) -> Result<Vec<u8>, CoreError>;
}

/// `Idle` and `Offline` carry unix-ms timestamps; `last_seen = 0` means unknown.
#[derive(uniffi::Enum, Debug, Clone)]
pub enum Presence {
    Online,
    Idle { since: u64 },
    Offline { last_seen: u64 },
}

#[uniffi::export(with_foreign)]
pub trait CoreEvents: Send + Sync {
    fn on_connection(&self, state: ConnectionState);
    fn on_message(&self, event: MessageEvent);
    /// `activity` is an `ACTIVITY_*` bitset, `0` meaning idle. Ephemeral and never stored.
    fn on_activity(&self, conversation: Vec<u8>, peer: Vec<u8>, activity: u16);
    fn on_presence(&self, peer: Vec<u8>, presence: Presence);
    /// `reactor` is the author's IPK, `dispatch_id` the reacted message; `add` is false on removal.
    fn on_reaction(&self, conversation: Vec<u8>, dispatch_id: Vec<u8>, reactor: Vec<u8>, emoji: String, add: bool);
    /// A UI-facing DB committed a write to `tables`; the client re-runs overlapping queries. Fired
    /// on the writer thread, so the impl must not block or re-enter core.
    fn on_db_changed(&self, tables: Vec<String>);
    /// The platform owns the ringing screen, the service and the audio device; core owns the rest.
    fn on_call(&self, event: CallEvent);
    /// One H.264 access unit from the peer, Annex-B framed. The impl must not block.
    fn on_call_video(&self, frame: Vec<u8>, keyframe: bool);
    /// The peer asked for a keyframe or a receiver just came up. No-op outside a video call.
    fn on_call_video_keyframe(&self);
    /// Target encoder bitrate from the bandwidth estimate.
    fn on_call_video_bitrate(&self, kbps: u32);
}

/// `call` is the 16-byte id every call API takes; `conversation` is where the call row lands.
#[derive(uniffi::Enum, Debug, Clone)]
pub enum CallEvent {
    /// We started a call. Show the outgoing screen and hold the service.
    Outgoing { call: Vec<u8>, peer: Vec<u8>, conversation: Vec<u8> },
    /// Someone is calling. Ring, and show the incoming screen.
    Incoming { call: Vec<u8>, peer: Vec<u8>, conversation: Vec<u8>, video: bool },
    /// The peer's phone is ringing, so play ringback.
    Ringing { call: Vec<u8> },
    /// Our offer crossed the peer's and theirs won: our call `from` is now their call `to`, being
    /// answered. It comes before any event for `to`. `video` is theirs, not a reason to turn our
    /// camera on.
    Switched { from: Vec<u8>, to: Vec<u8>, video: bool },
    /// Answered on both ends; media is being set up.
    Connecting { call: Vec<u8> },
    /// Media flows. Run the audio device from here until `Ended`.
    Connected { call: Vec<u8> },
    /// The path dropped and recovery is under way; audio keeps running.
    Reconnecting { call: Vec<u8> },
    PeerMuted { call: Vec<u8>, muted: bool },
    PeerCamera { call: Vec<u8>, on: bool },
    /// Stop audio and dismiss the screen. `duration_ms` is the connected time, zero if it never
    /// connected. A `Missed` call that never rang here still reports, for the missed-call notice.
    Ended {
        call: Vec<u8>,
        peer: Vec<u8>,
        conversation: Vec<u8>,
        reason: CallEndReason,
        duration_ms: u64,
    },
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallEndReason {
    /// Hung up by either side after it connected.
    Hangup,
    /// We hung up before the peer answered.
    Cancelled,
    /// The callee refused (the peer, or us).
    Declined,
    Busy,
    /// We called and nobody picked up.
    Unanswered,
    /// We were called and did not pick up.
    Missed,
    /// The media path never came up or never recovered.
    Failed,
}

/// The single error type crossing the FFI boundary.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{msg}")]
    Internal { msg: String },
    /// A failure with a message suitable for display to the user.
    #[error("{msg}")]
    Refused { msg: String },
}

/// Preserve a user-facing message through anyhow as [`CoreError::Refused`].
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        if let Some(r) = e.downcast_ref::<Refused>() {
            return CoreError::Refused { msg: r.0.clone() };
        }
        // `{:#}` keeps the whole context chain, not just the outermost layer.
        CoreError::Internal { msg: format!("{e:#}") }
    }
}

/// FFI projection of [`MessageEv`].
#[derive(uniffi::Enum)]
pub enum MessageEvent {
    Received { id: String, conversation: Vec<u8>, sender: Vec<u8>, content: String, timestamp: u64 },
    Sent { id: String, conversation: Vec<u8>, content: String, timestamp: u64 },
    Failed { id: String, conversation: Vec<u8>, reason: String },
    Edited { id: String, conversation: Vec<u8>, content: String },
    Deleted { id: String, conversation: Vec<u8> },
}

impl From<MessageEv> for MessageEvent {
    fn from(e: MessageEv) -> Self {
        match e {
            MessageEv::Received { id, conversation, sender, content, timestamp } => {
                MessageEvent::Received {
                    id: id.to_string(),
                    conversation: conversation.to_vec(),
                    sender: sender.to_vec(),
                    content,
                    timestamp,
                }
            },
            MessageEv::Sent { id, conversation, content, timestamp } => {
                MessageEvent::Sent {
                    id: id.to_string(),
                    conversation: conversation.to_vec(),
                    content,
                    timestamp,
                }
            },
            MessageEv::Failed { id, conversation, reason } => {
                MessageEvent::Failed { id: id.to_string(), conversation: conversation.to_vec(), reason }
            },
            MessageEv::Edited { id, conversation, content } => {
                MessageEvent::Edited { id: id.to_string(), conversation: conversation.to_vec(), content }
            },
            MessageEv::Deleted { id, conversation } => {
                MessageEvent::Deleted { id: id.to_string(), conversation: conversation.to_vec() }
            },
        }
    }
}
