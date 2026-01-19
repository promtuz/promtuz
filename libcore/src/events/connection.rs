use std::sync::atomic::Ordering;

use serde::Serialize;

use crate::api::conn_stats::CONNECTION_STATE;
use crate::events::Emittable;
use crate::events::InternalEvent;

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[allow(unused)]
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
}

impl Emittable for ConnectionState {
    fn emit(self) {
        CONNECTION_STATE.store(self.clone() as i32, Ordering::Relaxed);

        InternalEvent::emit("CONNECTION", &self);
    }
}
