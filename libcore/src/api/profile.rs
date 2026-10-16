//! Own profile, and the pictures others told us. Pictures cross the FFI as AVIF bytes both ways,
//! so the platform decodes them like an inline image.

use common::types::bytes::fixed;
use common::utils::now_ms;

use crate::data::identity::Identity;
use crate::platform::CoreError;
use crate::state::core;

#[uniffi::export]
pub fn profile_identity() -> Vec<u8> { Identity::get().map(|i| i.ipk().to_vec()).unwrap_or_default() }

#[uniffi::export]
pub fn profile_name() -> String {
    Identity::get().map(|i| i.name()).unwrap_or_default()
}

#[uniffi::export]
pub fn profile_picture() -> Option<Vec<u8>> {
    Identity::get().and_then(|i| i.avatar())
}

#[uniffi::export]
pub fn set_profile_picture_encoded(avif: Vec<u8>) -> Result<(), CoreError> {
    let image = crate::media::image::validate_avatar_avif(&avif)
        .map_err(|e| CoreError::Refused { msg: e.to_string() })?;
    save_profile_picture(Some(image.bytes))
}

#[uniffi::export]
pub fn clear_profile_picture() -> Result<(), CoreError> {
    save_profile_picture(None)
}

fn save_profile_picture(avif: Option<Vec<u8>>) -> Result<(), CoreError> {
    Identity::set_avatar(avif.as_deref())?;
    Ok(())
}

/// Moves whenever any picture changes. Caching clients compare it instead of re-decoding on every
/// messages-DB doorbell, which rings for every message.
#[uniffi::export]
pub fn avatar_generation() -> u64 {
    crate::data::peer_avatar::generation()
}

/// What `ipk` last told a chat we share, or our own picture; `None` means draw initials.
#[uniffi::export]
pub fn avatar_of(ipk: Vec<u8>) -> Result<Option<Vec<u8>>, CoreError> {
    let who = fixed::<32>(&ipk, "ipk")?;
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
    let who = fixed::<32>(&ipk, "ipk")?;
    let profile = Identity::get().filter(|i| i.ipk() == who).map(|i| i.details())
        .or_else(|| crate::data::peer_profile::get(&who));
    Ok(PersonProfile {
        name: profile
            .as_ref()
            .filter(|p| !p.name.is_empty())
            .map(|p| p.name.clone())
            .or_else(|| crate::data::peer_name::named(&who).map(|(name, _)| name))
            .unwrap_or_default(),
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
    Ok(())
}

#[uniffi::export]
pub fn set_contact_nickname(ipk: Vec<u8>, nickname: String) -> Result<(), CoreError> {
    let who = fixed::<32>(&ipk, "ipk")?;
    if !crate::data::contact::Contact::exists(&who) { return Err(anyhow::anyhow!("contact not found").into()); }
    if nickname.chars().count() > 32 { return Err(anyhow::anyhow!("Nickname is limited to 32 characters").into()); }
    crate::data::app_prefs::set(&format!("nickname:{}", hex::encode(who)), nickname.trim())?;
    crate::data::peer_profile::notify_changed();
    Ok(())
}

#[uniffi::export]
pub fn group_picture(conversation_id: Vec<u8>) -> Result<Option<Vec<u8>>, CoreError> {
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    Ok(crate::data::group_picture::snapshot(&conv).and_then(|(_, avif)| avif))
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn set_group_picture_encoded(
    conversation_id: Vec<u8>, avif: Vec<u8>,
) -> Result<(), CoreError> {
    update_group_picture(conversation_id, move || {
        let image = crate::media::image::validate_avatar_avif(&avif)
            .map_err(|e| CoreError::Refused { msg: e.to_string() })?;
        Ok(Some(image.bytes))
    })
    .await
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn clear_group_picture(conversation_id: Vec<u8>) -> Result<(), CoreError> {
    update_group_picture(conversation_id, || Ok(None)).await
}

/// Both input routes share the permission check, serialized revision allocation
/// and durable update. Only the prepared payload changes between them.
async fn update_group_picture(
    conversation_id: Vec<u8>, prepare: impl FnOnce() -> Result<Option<Vec<u8>>, CoreError> + Send,
) -> Result<(), CoreError> {
    let _one = core().groups.picture_write.lock().await;
    let conv = fixed::<16>(&conversation_id, "conversation id")?;
    let me = Identity::local_ipk().ok_or_else(|| anyhow::anyhow!("no identity"))?;
    if !crate::data::conversation::Conversation::may_edit(&conv, &me) {
        return Err(anyhow::anyhow!("only admins can change the group's photo").into());
    }
    let avif = prepare()?;
    let revision = crate::data::group_picture::snapshot(&conv)
        .map(|(r, _)| r)
        .unwrap_or(0)
        .max(now_ms())
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("revision overflow"))?;
    crate::data::group_picture::receive(conv, me, revision, avif.clone())?;
    core().spawn(async move {
        let _ = crate::messaging::send_control(
            conv,
            common::proto::mls_wire::AppPayload::GroupPicture { revision, avif },
        )
        .await;
    });
    Ok(())
}
