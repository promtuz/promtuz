//! Core-to-client events, delivered through the `platform::CoreEvents` sink set at `api::init`.

pub mod connection;
pub mod messaging;

pub trait Emittable {
    fn emit(self);
}
