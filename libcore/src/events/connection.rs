use crate::events::Emittable;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
#[repr(i32)]
pub enum ConnectionState {
    Disconnected,
    Idle,
    Resolving,
    Connecting,
    Handshaking,
    Connected,
    Reconnecting,
    Failed,
    NoInternet,
    /// Link and auth are up while the offline backlog is still being pulled into the local DB.
    /// Appended last because the client maps variants by ordinal.
    Syncing,
}

impl Emittable for ConnectionState {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_connection(self);
        }
    }
}
