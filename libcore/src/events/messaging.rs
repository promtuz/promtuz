use crate::db::utils::ulid::ULID;
use crate::events::Emittable;

#[derive(Debug, Clone)]
pub enum MessageEv {
    /// `sender` is the member who wrote it, which in a group is not the conversation.
    Received {
        id: ULID,
        conversation: [u8; 16],
        sender: [u8; 32],
        content: String,
        timestamp: u64,
    },
    /// Our sent message was accepted by the relay
    Sent {
        id: ULID,
        conversation: [u8; 16],
        content: String,
        timestamp: u64,
    },
    Failed {
        id: ULID,
        conversation: [u8; 16],
        reason: String,
    },
    /// Our edit or an inbound peer edit.
    Edited {
        id: ULID,
        conversation: [u8; 16],
        content: String,
    },
    /// Tombstoned for everyone, or removed for me.
    Deleted {
        id: ULID,
        conversation: [u8; 16],
    },
}

impl Emittable for MessageEv {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_message(self.into());
        }
    }
}

/// Ephemeral and never stored. `activity` is an OR of `client_rel::ACTIVITY_*` bits; `0` means
/// present but idle.
#[derive(Debug, Clone)]
pub struct ActivityEv {
    pub conversation: [u8; 16],
    pub peer: [u8; 32],
    pub activity: u16,
}

impl Emittable for ActivityEv {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_activity(self.conversation.to_vec(), self.peer.to_vec(), self.activity);
        }
    }
}

#[derive(Debug, Clone)]
pub struct PresenceEv {
    pub peer: [u8; 32],
    pub presence: crate::platform::Presence,
}

impl Emittable for PresenceEv {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_presence(self.peer.to_vec(), self.presence);
        }
    }
}

/// `reactor` is the author's IPK; `add` is false when the reaction is removed.
#[derive(Debug, Clone)]
pub struct ReactionEv {
    pub conversation: [u8; 16],
    pub dispatch_id: [u8; 16],
    pub reactor: [u8; 32],
    pub emoji: String,
    pub add: bool,
}

impl Emittable for ReactionEv {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_reaction(self.conversation.to_vec(), self.dispatch_id.to_vec(), self.reactor.to_vec(), self.emoji, self.add);
        }
    }
}
