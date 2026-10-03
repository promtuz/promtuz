//! Composer staging: FFI for the media buffer that sits in front of a send.

use common::types::bytes::fixed;

use crate::platform::CoreError;
use crate::state::core;

/// Carries no encoded bytes: the client previews from its picked URI. `thumb` is the exception,
/// since an attachment has no client-side preview.
#[derive(uniffi::Record)]
pub struct StagedRecord {
    pub id:     u64,
    /// 1 = image, 2 = attachment (the media side-row kinds).
    pub kind:   u8,
    /// 0 = preparing, 1 = ready, 2 = failed.
    pub state:  u8,
    pub mime:   String,
    pub name:   String,
    pub size:   u64,
    pub width:  u32,
    pub height: u32,
    pub thumb:  Option<Vec<u8>>,
    /// Why the prepare failed, when `state` is 2.
    pub error:  Option<String>,
}

impl From<crate::staging::Staged> for StagedRecord {
    fn from(s: crate::staging::Staged) -> Self {
        StagedRecord {
            id:     s.id,
            kind:   s.kind,
            state:  s.state,
            mime:   s.mime,
            name:   s.name,
            size:   s.size,
            width:  s.width,
            height: s.height,
            thumb:  s.thumb,
            error:  s.error,
        }
    }
}

/// Returns the id at once; the AVIF pass runs off-thread and the item turns ready, or failed when
/// over budget, through the `"staging"` doorbell.
#[uniffi::export]
pub fn stage_image(rgba: Vec<u8>, width: u32, height: u32) -> u64 {
    crate::staging::stage_image(rgba, width, height)
}

/// Keep an already prepared AVIF intact. Larger images use the existing attachment path.
#[uniffi::export]
pub fn stage_encoded_image(bytes: Vec<u8>) -> Result<u64, CoreError> {
    Ok(crate::staging::stage_encoded_image(bytes)?)
}

/// The blurred preview is ready when this returns; the manifest hash runs off-thread.
#[uniffi::export]
pub fn stage_attachment(
    source_path: String, name: String, mime: String, thumb_rgba: Option<Vec<u8>>, thumb_w: u32,
    thumb_h: u32,
) -> Result<u64, CoreError> {
    Ok(crate::staging::stage_attachment(source_path, name, mime, thumb_rgba, thumb_w, thumb_h)?)
}

/// Safe mid-prepare: the running pass finds the id gone and drops its result.
#[uniffi::export]
pub fn discard_staged(id: u64) {
    crate::staging::discard(id);
}

/// The buffer's contents, in the order they'll send.
#[uniffi::export]
pub fn staged_items() -> Vec<StagedRecord> {
    crate::staging::list().into_iter().map(Into::into).collect()
}

/// One album, or a lone message, with `caption` on the first item. Every id must be ready or the
/// send fails; outcomes arrive via `on_message`.
#[uniffi::export]
pub fn send_staged(
    conversation_id: Vec<u8>, ids: Vec<u64>, caption: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let reply = reply_to.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    core().spawn(async move {
        if let Err(e) = crate::staging::commit(to, ids, caption, reply).await {
            log::error!("STAGING: commit failed: {e}");
        }
    });
    Ok(())
}

/// Waits for durable message creation before the caller releases its staged ids.
/// Network delivery continues independently after each message is committed.
#[uniffi::export(async_runtime = "tokio")]
pub async fn commit_staged(
    conversation_id: Vec<u8>, ids: Vec<u64>, caption: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let reply = reply_to.as_deref().map(|b| fixed::<16>(b, "dispatch_id")).transpose()?;
    crate::api::messaging::on_runtime(async move {
        crate::staging::commit(to, ids, caption, reply).await
    })
    .await
}

/// Replaces a prior message's body with a staged item. An illegal swap is refused, and the item
/// stays in the buffer either way.
#[uniffi::export]
pub fn revise_with_staged(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, staged_id: u64, caption: String,
) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    let target = fixed::<16>(&dispatch_id, "dispatch_id")?;
    let body = crate::staging::body_of(staged_id, caption)?;
    // Applied before returning: the caller clears the buffer next, which unlinks an attachment no
    // message names yet.
    if let Some((row, content)) =
        crate::messaging::body::apply_revise_body(&to, &target, body.clone(), true, None)?
    {
        use crate::events::Emittable;
        crate::events::messaging::MessageEv::Edited { id: row.id, conversation: to, content }.emit();
    }
    core().spawn(async move {
        if let Err(e) = crate::messaging::send_control(
            to,
            common::proto::mls_wire::AppPayload::Revise { target, body },
        )
        .await
        {
            log::error!("STAGING: revise failed: {e}");
        }
    });
    Ok(())
}

/// Share sheet waits for durable message creation before releasing its staged files.
#[uniffi::export(async_runtime = "tokio")]
pub async fn commit_shared(conversation_id: Vec<u8>, ids: Vec<u64>, caption: String) -> Result<(), CoreError> {
    let to = fixed::<16>(&conversation_id, "conversation id")?;
    crate::api::messaging::on_runtime(async move {
        if ids.is_empty() {
            anyhow::ensure!(!caption.trim().is_empty(), "nothing to share");
            let message = crate::data::message::Message::save_outgoing(to, &caption, None)?;
            let payload = crate::messaging::body::rebuild_pending_payload(&to, &message)?;
            core().spawn(async move {
                if let Err(e) = crate::messaging::send_prepared(to, &message, payload).await {
                    log::debug!("SHARE: text remains pending: {e}");
                }
            });
            Ok(())
        } else {
            crate::staging::commit(to, ids, caption, None).await
        }
    })
    .await
}
