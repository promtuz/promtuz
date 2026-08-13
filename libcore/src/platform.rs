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
}

/// The single error type crossing the FFI boundary.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("{msg}")]
    Internal { msg: String },
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
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
