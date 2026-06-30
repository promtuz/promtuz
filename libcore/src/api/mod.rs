//! FFI translation layer.
//!
//! A leaf: it depends on the core engine (`messaging`, `data`, `mls`,
//! `quic`, `state`, …) and nothing depends on it. uniffi is the glue —
//! this module only translates between the native core and clients.
//!
//! Rebuilt from scratch in the manual-JNI → uniffi migration (#3). The
//! old hand-rolled JNI surface is archived at
//! `../.archive/libcore-jni-api/` for reference while this is filled in.

pub mod identity;
pub mod init;
pub mod messaging;
pub mod qr;

use crate::data::identity::Identity;

/// Whether the client should launch straight into the app (an identity
/// exists) or show enrollment first.
#[uniffi::export]
pub fn should_launch_app() -> bool {
    Identity::public_key().is_ok()
}
