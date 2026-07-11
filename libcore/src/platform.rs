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
    fn on_activity(&self, peer: Vec<u8>, activity: u16);
    /// A contact's presence changed (online / idle-since / offline-last-seen).
    fn on_presence(&self, peer: Vec<u8>, presence: Presence);
    /// A reaction was added (`add = true`) or removed on a message. `reactor`
    /// is the author's IPK — compare to self for "mine". `peer` is the
    /// conversation, `dispatch_id` the reacted message.
    fn on_reaction(&self, peer: Vec<u8>, dispatch_id: Vec<u8>, reactor: Vec<u8>, emoji: String, add: bool);
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
        CoreError::Internal { msg: e.to_string() }
    }
}

/// Boundary projection of the domain [`MessageEv`]: `ULID` → `String`,
/// raw IPK → bytes. Kept distinct from `MessageEv` so the domain event
/// stays ergonomic and only the wire shape is FFI-constrained.
#[derive(uniffi::Enum)]
pub enum MessageEvent {
    Received { id: String, from: Vec<u8>, content: String, timestamp: u64 },
    Sent { id: String, to: Vec<u8>, content: String, timestamp: u64 },
    Failed { id: String, to: Vec<u8>, reason: String },
    Edited { id: String, peer: Vec<u8>, content: String },
    Deleted { id: String, peer: Vec<u8> },
    /// Peer acknowledged our messages up to `upto` (dispatch_id) at `status`
    /// (3 = delivered, 4 = read). UI bumps all rendered messages ≤ upto.
    Receipt { peer: Vec<u8>, upto: Vec<u8>, status: u8 },
}

impl From<MessageEv> for MessageEvent {
    fn from(e: MessageEv) -> Self {
        match e {
            MessageEv::Received { id, from, content, timestamp } => {
                MessageEvent::Received { id: id.to_string(), from: from.to_vec(), content, timestamp }
            },
            MessageEv::Sent { id, to, content, timestamp } => {
                MessageEvent::Sent { id: id.to_string(), to: to.to_vec(), content, timestamp }
            },
            MessageEv::Failed { id, to, reason } => {
                MessageEvent::Failed { id: id.to_string(), to: to.to_vec(), reason }
            },
            MessageEv::Edited { id, peer, content } => {
                MessageEvent::Edited { id: id.to_string(), peer: peer.to_vec(), content }
            },
            MessageEv::Deleted { id, peer } => {
                MessageEvent::Deleted { id: id.to_string(), peer: peer.to_vec() }
            },
            MessageEv::Receipt { peer, upto, status } => {
                MessageEvent::Receipt { peer: peer.to_vec(), upto: upto.to_vec(), status }
            },
        }
    }
}

/// Client-supplied key store, installed once at [`crate::api::init`].
pub static SECURE_STORE: OnceCell<Arc<dyn SecureStore>> = OnceCell::new();

/// Client-supplied event sink, installed once at [`crate::api::init`].
pub static EVENTS: OnceCell<Arc<dyn CoreEvents>> = OnceCell::new();
