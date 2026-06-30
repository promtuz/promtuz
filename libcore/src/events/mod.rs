//! Internal events — core → client notifications, delivered through the
//! `platform::CoreEvents` sink the client installs at `api::init`. (The
//! old CBOR-over-`onEvent` JNI callback is gone.)

pub mod connection;
pub mod messaging;

/// A core event that pushes itself to the client event sink.
pub trait Emittable {
    fn emit(self);
}
