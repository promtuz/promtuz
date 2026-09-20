//! Own profile: the picture beside the name, and the pictures others told us.
//!
//! The name already has a home (`enroll`, the recovery flows); this is what a
//! profile screen needs on top of it. Pictures cross the FFI as AVIF bytes in
//! both directions, so the platform decodes the same way it does for an inline
//! image and never sees a second format.

use crate::api::messaging::to_ipk32;
use crate::data::identity::Identity;
use crate::platform::CoreError;

/// Our display name, as enrolled or last restored.
#[uniffi::export]
pub fn profile_name() -> String {
    Identity::get().map(|i| i.name()).unwrap_or_default()
}

/// Our profile picture as AVIF bytes, or `None` when we have none.
#[uniffi::export]
pub fn profile_picture() -> Option<Vec<u8>> {
    Identity::get().and_then(|i| i.avatar())
}

/// Set the profile picture from platform-decoded RGBA. Square-cropped, scaled
/// and AVIF-encoded here so every platform ships the same bytes, then stored
/// and told to every chat we are in.
///
/// Blocks on the encode (tens of milliseconds at avatar size) and returns once
/// the picture is stored. The sends are fire-and-forget like every control
/// message: each peer's copy lands on its own, the outbox covering anyone
/// offline.
#[uniffi::export]
pub fn set_profile_picture(rgba: Vec<u8>, width: u32, height: u32) -> Result<(), CoreError> {
    let avif = crate::media::avatar_from_rgba(&rgba, width, height)?;
    Identity::set_avatar(Some(&avif))?;
    crate::messaging::broadcast_avatar(Some(avif));
    Ok(())
}

/// Remove the profile picture, and tell every chat to stop showing it.
#[uniffi::export]
pub fn clear_profile_picture() -> Result<(), CoreError> {
    Identity::set_avatar(None)?;
    crate::messaging::broadcast_avatar(None);
    Ok(())
}

/// Moves whenever a picture changes, ours or a peer's. The DB doorbell rings
/// for every commit in the messages DB, which is every message; a client that
/// caches decoded pictures compares this instead and re-decodes only when one
/// actually changed.
#[uniffi::export]
pub fn avatar_generation() -> u64 {
    crate::data::peer_avatar::generation()
}

/// The picture to draw for `ipk`: our own for ourselves, otherwise what that
/// person last told a chat we share. `None` means draw initials.
#[uniffi::export]
pub fn avatar_of(ipk: Vec<u8>) -> Result<Option<Vec<u8>>, CoreError> {
    let who = to_ipk32(&ipk)?;
    if let Some(me) = Identity::get()
        && me.ipk() == who
    {
        return Ok(me.avatar());
    }
    Ok(crate::data::peer_avatar::get(&who))
}
