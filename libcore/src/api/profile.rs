//! Own profile: the picture beside the name, and the pictures others told us.
//!
//! The name already has a home (`enroll`, the recovery flows); this is what a
//! profile screen needs on top of it. Pictures cross the FFI as AVIF bytes in
//! both directions, so the platform decodes the same way it does for an inline
//! image and never sees a second format.

use crate::api::messaging::to_ipk32;
use crate::data::identity::Identity;
use crate::data::peer_avatar::AvatarUpdate;
use crate::platform::CoreError;

#[uniffi::export]
pub fn profile_identity() -> Vec<u8> { Identity::get().map(|i| i.ipk().to_vec()).unwrap_or_default() }

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
    let revision = Identity::set_avatar(Some(&avif))?;
    crate::messaging::broadcast_avatar(AvatarUpdate { revision, avif: Some(avif) });
    Ok(())
}

/// Remove the profile picture, and tell every chat to stop showing it.
#[uniffi::export]
pub fn clear_profile_picture() -> Result<(), CoreError> {
    let revision = Identity::set_avatar(None)?;
    crate::messaging::broadcast_avatar(AvatarUpdate { revision, avif: None });
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

#[derive(uniffi::Record)]
pub struct PersonProfile {
    pub name: String,
    pub bio: String,
    pub nickname: String,
    pub can_share: bool,
}

#[uniffi::export]
pub fn person_profile(ipk: Vec<u8>) -> Result<PersonProfile, CoreError> {
    let who = to_ipk32(&ipk)?;
    let profile = Identity::get().filter(|i| i.ipk() == who).map(|i| i.details())
        .or_else(|| crate::data::peer_profile::get(&who));
    Ok(PersonProfile {
        name: profile.as_ref().map(|p| p.name.clone()).or_else(|| crate::data::peer_name::get(&who))
            .or_else(|| crate::data::contact::Contact::get(&who).map(|c| c.inner.name.clone())).unwrap_or_default(),
        can_share: profile.as_ref().is_some_and(|p| !p.card.is_empty()),
        bio: profile.map(|p| p.bio).unwrap_or_default(),
        nickname: crate::data::app_prefs::get(&format!("nickname:{}", hex::encode(who))).unwrap_or_default(),
    })
}

#[uniffi::export]
pub fn profile_bio() -> String {
    Identity::get().map(|i| i.details().bio).unwrap_or_default()
}

#[uniffi::export]
pub fn set_profile_details(name: String, bio: String) -> Result<(), CoreError> {
    Identity::set_details(&name, &bio)?;
    if let Some(me) = Identity::get() {
        crate::messaging::broadcast_profile(me.details().into_payload());
        // Older peers still understand the name-only introduction.
        crate::messaging::broadcast_profile(common::proto::mls_wire::AppPayload::Profile { name: me.name() });
    }
    Ok(())
}

#[uniffi::export]
pub fn set_contact_nickname(ipk: Vec<u8>, nickname: String) -> Result<(), CoreError> {
    let who = to_ipk32(&ipk)?;
    if !crate::data::contact::Contact::exists(&who) { return Err(anyhow::anyhow!("contact not found").into()); }
    if nickname.chars().count() > 32 { return Err(anyhow::anyhow!("Nickname is limited to 32 characters").into()); }
    crate::data::app_prefs::set(&format!("nickname:{}", hex::encode(who)), nickname.trim())?;
    crate::data::peer_profile::notify_changed();
    Ok(())
}

#[uniffi::export]
pub fn group_picture(conversation_id: Vec<u8>) -> Result<Option<Vec<u8>>, CoreError> {
    let conv = super::messaging::to_conv16(&conversation_id)?;
    Ok(crate::data::group_picture::snapshot(&conv).and_then(|(_, avif)| avif))
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn set_group_picture(conversation_id: Vec<u8>, rgba: Option<Vec<u8>>, width: u32, height: u32) -> Result<(), CoreError> {
    static WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = WRITE.lock().await;
    let conv = super::messaging::to_conv16(&conversation_id)?;
    let me = Identity::get().ok_or_else(|| anyhow::anyhow!("no identity"))?.ipk();
    if !crate::data::conversation::Conversation::is_admin(&conv, &me) { return Err(anyhow::anyhow!("only the admin can change the picture").into()); }
    let avif = rgba.map(|bytes| crate::media::avatar_from_rgba(&bytes, width, height)).transpose()?;
    let revision = crate::data::group_picture::snapshot(&conv).map(|(r, _)| r).unwrap_or(0)
        .max(crate::utils::systime().as_millis() as u64).checked_add(1).ok_or_else(|| anyhow::anyhow!("revision overflow"))?;
    crate::data::group_picture::receive(conv, me, revision, avif.clone())?;
    crate::RUNTIME.spawn(async move {
        let _ = crate::messaging::send_control(conv, common::proto::mls_wire::AppPayload::GroupPicture { revision, avif }).await;
    });
    Ok(())
}
