use common::msg::cbor::ToCbor;
use jni::objects::GlobalRef;
use log::trace;
use parking_lot::Mutex;
use serde::Serialize;

use crate::JVM;
use crate::events::connection::ConnectionState;
use crate::events::identity::Identity;

pub mod callback;
pub mod connection;
pub mod identity;

pub static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);

pub trait Emittable {
    fn emit(self);
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[allow(unused)]
pub enum InternalEvent {
    Connection { state: ConnectionState },
    Identity { event: Identity },
}

pub fn emit_event(event: InternalEvent) {
    trace!("EVENT_EMIT: {:?}", event);

    let event_bytes = event.to_cbor().unwrap();

    let vm = JVM.get().expect("JVM not initialized");
    let mut env = vm.attach_current_thread().unwrap();

    if let Some(callback) = CALLBACK.lock().as_ref() {
        let arr = &env.byte_array_from_slice(&event_bytes).unwrap();
        env.call_method(callback.as_obj(), "onEvent", "([B)V", &[arr.into()]).unwrap();
    }
}
