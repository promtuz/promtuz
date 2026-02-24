//! Internal Events used for inter-op between ui and libcore
//! Should not be used for events transmitted over network

use std::fmt::Debug;

use jni::objects::GlobalRef;
use parking_lot::Mutex;
use serde::Serialize;

use crate::JVM;

pub mod callback;
pub mod connection;
pub mod identity;
pub mod messaging;

pub static CALLBACK: Mutex<Option<GlobalRef>> = Mutex::new(None);

pub trait Emittable: Serialize {
    fn emit(self);
}

#[allow(unused)]
#[derive(Serialize, Debug, Clone)]
pub struct InternalEvent {
    // Connection { state: ConnectionState },
    // Identity { event: IdentityEv },
}

impl InternalEvent {
    fn emit<V: Emittable + Sized + Debug + 'static>(tag: &str, val: &V) {
        if tag.len() > u8::MAX.into() {
            return log::error!("ERROR: InternalEvent tag too long, ignoring!");
        }

        let mut value_encoded = vec![0u8; 0];

        ciborium::into_writer(&val, &mut value_encoded).unwrap();

        let mut event_bytes = vec![0u8; 0];

        // Event tag length and string
        event_bytes.push(tag.len().try_into().unwrap());
        
        event_bytes = [&event_bytes, tag.as_bytes()].concat();

        event_bytes = [&event_bytes as &[u8], &value_encoded].concat();

        log::trace!("TRACE: InternalEvent({tag}): {val:?}");

        let vm = JVM.get().expect("JVM not initialized");
        let mut env = vm.attach_current_thread().unwrap();

        if let Some(callback) = CALLBACK.lock().as_ref() {
            let arr = &env.byte_array_from_slice(&event_bytes).unwrap();
            env.call_method(callback.as_obj(), "onEvent", "([B)V", &[arr.into()]).unwrap();
        }
    }
}

// 0a434f4e4e454354494f4e0c00
