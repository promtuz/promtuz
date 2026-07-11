use serde::Serialize;
use crate::db::utils::ulid::ULID;
use crate::events::Emittable;

#[derive(Serialize, Debug, Clone)]
pub enum MessageEv {
    /// A new message was received and decrypted
    Received {
        id: ULID,
        #[serde(with = "serde_bytes")]
        from: [u8; 32],
        content: String,
        timestamp: u64,
    },
    /// Our sent message was accepted by the relay
    Sent {
        id: ULID,
        #[serde(with = "serde_bytes")]
        to: [u8; 32],
        content: String,
        timestamp: u64,
    },
    /// Our sent message failed
    Failed {
        id: ULID,
        #[serde(with = "serde_bytes")]
        to: [u8; 32],
        reason: String,
    },
    /// A message's text changed (our edit, or an inbound peer Edit).
    Edited {
        id: ULID,
        #[serde(with = "serde_bytes")]
        peer: [u8; 32],
        content: String,
    },
    /// A message was deleted (tombstoned for-everyone, or removed for-me).
    Deleted {
        id: ULID,
        #[serde(with = "serde_bytes")]
        peer: [u8; 32],
    },
    /// The peer acknowledged our outgoing messages up to `upto` (a 16-byte
    /// dispatch_id) at `status` (Delivered/Read). High-water-mark: the UI
    /// bumps every rendered message with `dispatch_id <= upto` to `status`.
    Receipt {
        #[serde(with = "serde_bytes")]
        peer: [u8; 32],
        #[serde(with = "serde_bytes")]
        upto: [u8; 16],
        status: u8,
    },
}

impl Emittable for MessageEv {
    fn emit(self) {
        if let Some(events) = crate::platform::EVENTS.get() {
            events.on_message(self.into());
        }
    }
}

/// A contact's live activity changed — an ephemeral, unstored signal.
/// `activity` is an OR of `common::proto::client_rel::ACTIVITY_*` bits;
/// `0` = present-but-idle. The UI decides how to render (typing dots, etc.).
#[derive(Debug, Clone)]
pub struct ActivityEv {
    pub peer: [u8; 32],
    pub activity: u16,
}

impl Emittable for ActivityEv {
    fn emit(self) {
        if let Some(events) = crate::platform::EVENTS.get() {
            events.on_activity(self.peer.to_vec(), self.activity);
        }
    }
}

/// A contact's presence changed (online / idle-since / offline-last-seen).
#[derive(Debug, Clone)]
pub struct PresenceEv {
    pub peer: [u8; 32],
    pub presence: crate::platform::Presence,
}

impl Emittable for PresenceEv {
    fn emit(self) {
        if let Some(events) = crate::platform::EVENTS.get() {
            events.on_presence(self.peer.to_vec(), self.presence);
        }
    }
}

/// A reaction was added or removed. `reactor` is the author's IPK (compare to
/// self for "mine"); `add` distinguishes add from remove. Group-ready — each
/// member's reaction carries its own `reactor`.
#[derive(Debug, Clone)]
pub struct ReactionEv {
    pub peer: [u8; 32],
    pub dispatch_id: [u8; 16],
    pub reactor: [u8; 32],
    pub emoji: String,
    pub add: bool,
}

impl Emittable for ReactionEv {
    fn emit(self) {
        if let Some(events) = crate::platform::EVENTS.get() {
            events.on_reaction(self.peer.to_vec(), self.dispatch_id.to_vec(), self.reactor.to_vec(), self.emoji, self.add);
        }
    }
}
