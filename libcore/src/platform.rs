//! Platform ports — the contracts the core engine needs *from* the host
//! client (key custody, event delivery) plus the error/DTO types those
//! contracts speak in.
//!
//! These live here, not in `api`, on purpose: the engine
//! (`data`, `messaging`, `quic`, …) depends on them, and the engine must
//! never depend on the FFI layer. uniffi exposes the traits as
//! foreign-implementable interfaces; the client supplies concrete impls
//! once, at [`crate::api::init`].

use std::sync::Arc;

use once_cell::sync::OnceCell;

use crate::events::connection::ConnectionState;
use crate::events::messaging::MessageEv;

/// Hardware-backed secret custody. The client seals/opens key material
/// with a platform key store (Android Keystore, iOS Keychain, a TPM, an
/// OS keyring …). Crypto stays in core — only *custody* of the wrapping
/// key crosses the boundary.
#[uniffi::export(with_foreign)]
pub trait SecureStore: Send + Sync {
    fn seal(&self, plaintext: Vec<u8>) -> Result<Vec<u8>, CoreError>;
    fn open(&self, ciphertext: Vec<u8>) -> Result<Vec<u8>, CoreError>;
}

/// A contact's presence, for the client. `Idle`/`Offline` carry a unix-ms
/// timestamp (`Offline.last_seen = 0` means unknown).
#[derive(uniffi::Enum, Debug, Clone)]
pub enum Presence {
    Online,
    Idle { since: u64 },
    Offline { last_seen: u64 },
}

/// Typed event delivery to the client — replaces the old single
/// CBOR-over-`onEvent` callback. The client implements it; core calls it.
#[uniffi::export(with_foreign)]
pub trait CoreEvents: Send + Sync {
    fn on_connection(&self, state: ConnectionState);
    fn on_message(&self, event: MessageEvent);
    /// A contact's live activity (typing/recording/… bitset; 0 = idle/online).
    /// Ephemeral — never stored; drop if the peer isn't in the current view.
    fn on_activity(&self, conversation: Vec<u8>, peer: Vec<u8>, activity: u16);
    /// A contact's presence changed (online / idle-since / offline-last-seen).
    fn on_presence(&self, peer: Vec<u8>, presence: Presence);
    /// A reaction was added (`add = true`) or removed on a message. `reactor`
    /// is the author's IPK — compare to self for "mine". `conversation` is the
    /// chat scope, `dispatch_id` the reacted message.
    fn on_reaction(&self, conversation: Vec<u8>, dispatch_id: Vec<u8>, reactor: Vec<u8>, emoji: String, add: bool);
    /// A UI-facing DB committed a write — the coarse "re-read" doorbell for the
    /// reactive layer. `tables` names what moved (e.g. `["messages","reactions"]`);
    /// the client re-runs any observed query overlapping them. Content-free —
    /// truth stays in the DB. Fired on the writer thread, so the impl must not
    /// block or re-enter the core (just wake a flow).
    fn on_db_changed(&self, tables: Vec<String>);
    /// A call changed state. The platform owns the ringing screen, the
    /// foreground service and the audio device; core owns everything else.
    fn on_call(&self, event: CallEvent);
    /// One encoded H.264 access unit from the peer, Annex-B framed. The
    /// platform feeds it to its decoder and renders. Fires only in a video
    /// call, off the media thread, so the impl must not block.
    fn on_call_video(&self, frame: Vec<u8>, keyframe: bool);
    /// Our encoder should produce a keyframe now: the peer asked for one, or a
    /// receiver just came up. No-op outside a video call.
    fn on_call_video_keyframe(&self);
    /// The target send bitrate for our video encoder, in kbps, from the
    /// bandwidth estimate. The platform retunes MediaCodec to it.
    fn on_call_video_bitrate(&self, kbps: u32);
}

/// One step of a call, for the platform. `call` is the 16-byte call id every
/// call API takes; `peer` the other party's IPK; `conversation` the chat the
/// call row lands in.
#[derive(uniffi::Enum, Debug, Clone)]
pub enum CallEvent {
    /// We started a call. Show the outgoing screen and hold the service.
    Outgoing { call: Vec<u8>, peer: Vec<u8>, conversation: Vec<u8> },
    /// Someone is calling. Ring, and show the incoming screen.
    Incoming { call: Vec<u8>, peer: Vec<u8>, conversation: Vec<u8>, video: bool },
    /// The peer's phone is ringing, so play ringback.
    Ringing { call: Vec<u8> },
    /// Crossed calls resolved to the peer's: the call id changed. Re-key the
    /// screen and service from `from` to `to`; it is now an answered incoming
    /// call being connected, so `Connecting` follows immediately.
    Switched { from: Vec<u8>, to: Vec<u8>, video: bool },
    /// Answered on both ends; media is being set up.
    Connecting { call: Vec<u8> },
    /// Media flows. Run the audio device from here until `Ended`.
    Connected { call: Vec<u8> },
    /// The path dropped and recovery is under way; audio keeps running.
    Reconnecting { call: Vec<u8> },
    /// The peer muted or unmuted themselves.
    PeerMuted { call: Vec<u8>, muted: bool },
    /// The peer turned their camera on or off. When off, show their avatar.
    PeerCamera { call: Vec<u8>, on: bool },
    /// Over, however it went. Stop audio and dismiss the screen. `duration_ms`
    /// is the connected time, zero when it never connected. A `Missed` for a
    /// call that never rang here (the offer arrived already dead) still
    /// reports, so a missed-call notice can be shown.
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
    /// The callee was on another call.
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
        // `{:#}` joins the whole context chain. Plain `to_string()` prints only
        // the outermost layer, which is where the call happened rather than
        // what went wrong — "fetch_keypackage_for" instead of the reason.
        CoreError::Internal { msg: format!("{e:#}") }
    }
}

/// Boundary projection of the domain [`MessageEv`]: `ULID` → `String`,
/// raw IPK → bytes. Kept distinct from `MessageEv` so the domain event
/// stays ergonomic and only the wire shape is FFI-constrained.
#[derive(uniffi::Enum)]
pub enum MessageEvent {
    Received { id: String, conversation: Vec<u8>, sender: Vec<u8>, content: String, timestamp: u64 },
    Sent { id: String, conversation: Vec<u8>, content: String, timestamp: u64 },
    Failed { id: String, conversation: Vec<u8>, reason: String },
    Edited { id: String, conversation: Vec<u8>, content: String },
    Deleted { id: String, conversation: Vec<u8> },
    /// A member acknowledged our messages up to `upto` (dispatch_id) at
    /// `status` (3 = delivered, 4 = read). UI bumps all rendered messages
    /// ≤ upto; in a group the status only advances once every member has.
    Receipt { conversation: Vec<u8>, member: Vec<u8>, upto: Vec<u8>, status: u8 },
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
            MessageEv::Receipt { conversation, member, upto, status } => {
                MessageEvent::Receipt {
                    conversation: conversation.to_vec(),
                    member: member.to_vec(),
                    upto: upto.to_vec(),
                    status,
                }
            },
        }
    }
}

/// Client-supplied key store, installed once at [`crate::api::init`].
pub static SECURE_STORE: OnceCell<Arc<dyn SecureStore>> = OnceCell::new();

/// Client-supplied event sink, installed once at [`crate::api::init`].
pub static EVENTS: OnceCell<Arc<dyn CoreEvents>> = OnceCell::new();
