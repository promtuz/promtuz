//! Internal Events used for inter-op between ui and libcore
//! Should not be used for events transmitted over network 

use jni::objects::GlobalRef;
use log::trace;
use parking_lot::Mutex;
use serde::Serialize;

use crate::JVM;
use crate::events::connection::ConnectionState;
use crate::events::identity::IdentityEv;

pub mod callback;
pub mod connection;
pub mod identity;

pub static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);

pub trait Emittable {
    fn emit(self);
}

#[derive(Serialize, Debug, Clone)]
#[allow(unused)]
pub enum InternalEvent {
    Connection { state: ConnectionState },
    Identity { event: IdentityEv },
}

pub fn emit_event(event: InternalEvent) {
    trace!("EVENT_EMIT: {:?}", event);
    
    let mut event_bytes = vec![0u8; 0];
    ciborium::into_writer(&event, &mut event_bytes).unwrap();

    let vm = JVM.get().expect("JVM not initialized");
    let mut env = vm.attach_current_thread().unwrap();

    if let Some(callback) = CALLBACK.lock().as_ref() {
        let arr = &env.byte_array_from_slice(&event_bytes).unwrap();
        env.call_method(callback.as_obj(), "onEvent", "([B)V", &[arr.into()]).unwrap();
    }
}
