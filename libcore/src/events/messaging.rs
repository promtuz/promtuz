use serde::Serialize;

use crate::events::Emittable;
use crate::events::InternalEvent;

#[derive(Serialize, Debug, Clone)]
pub enum MessageEv {
    /// A new message was received and decrypted
    Received {
        #[serde(with = "serde_bytes")]
        from: [u8; 32],
        content: String,
        timestamp: u64,
    },
    /// Our sent message was accepted by the relay
    Sent {
        #[serde(with = "serde_bytes")]
        to: [u8; 32],
    },
    /// Our sent message failed
    Failed {
        #[serde(with = "serde_bytes")]
        to: [u8; 32],
        reason: String,
    },
}

impl Emittable for MessageEv {
    fn emit(self) {
        InternalEvent::emit("MESSAGE", &self);
    }
}
