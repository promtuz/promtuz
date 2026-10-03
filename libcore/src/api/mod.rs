//! FFI translation layer. The engine never depends on it.

pub mod identity;
pub mod init;
pub mod media;
pub mod messaging;
pub mod portable;
pub mod profile;
pub mod qr;
pub mod recovery;
pub mod relays;
pub mod staging;
pub mod stickers;
pub mod storage;
pub mod update;
pub mod video;

use crate::data::identity::Identity;

#[uniffi::export]
pub fn should_launch_app() -> bool {
    Identity::public_key().is_ok()
}
