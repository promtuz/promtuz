use crate::events::Emittable;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionState {
    Disconnected,
    Resolving,
    Connecting,
    Handshaking,
    Connected,
    Failed,
    /// Link and auth are up while the offline backlog is still being pulled into the local DB.
    Syncing,
}

impl Emittable for ConnectionState {
    fn emit(self) {
        if let Some(events) = crate::state::core().events.get() {
            events.on_connection(self);
        }
    }
}
