//! Composer staging: FFI for the media buffer that sits in front of a send.

use crate::api::messaging::to_did16;
use crate::api::messaging::to_conv16;
use crate::platform::CoreError;

/// A staged item, projected for the client.
///
/// Carries no encoded bytes on purpose: the client already holds the picked
/// URI and draws its own preview from that, so a 256KB AVIF crossing the FFI
/// per item — on every re-read of the buffer — would buy nothing. `thumb` is
/// the exception, since an attachment has no client-side preview to fall back
/// on.
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

/// Put a picked photo in the buffer. Returns its id at once — the AVIF pass
/// runs off-thread and the item flips to ready (or failed, for an over-budget
/// photo) through the `"staging"` re-read doorbell.
#[uniffi::export]
pub fn stage_image(rgba: Vec<u8>, width: u32, height: u32) -> u64 {
    crate::staging::stage_image(rgba, width, height)
}

/// Put a picked file in the buffer. The blurred preview is computed before this
/// returns so there's something to draw; the manifest hash runs off-thread.
#[uniffi::export]
pub fn stage_attachment(
    source_path: String, name: String, mime: String, thumb_rgba: Option<Vec<u8>>, thumb_w: u32,
    thumb_h: u32,
) -> Result<u64, CoreError> {
    Ok(crate::staging::stage_attachment(source_path, name, mime, thumb_rgba, thumb_w, thumb_h)?)
}

/// Remove one item from the buffer. Safe mid-prepare — the running pass finds
/// the id gone and drops its result.
#[uniffi::export]
pub fn discard_staged(id: u64) {
    crate::staging::discard(id);
}

/// Empty the buffer.
#[uniffi::export]
pub fn clear_staged() {
    crate::staging::clear();
}

/// The buffer's contents, in the order they'll send.
#[uniffi::export]
pub fn staged_items() -> Vec<StagedRecord> {
    crate::staging::list().into_iter().map(Into::into).collect()
}

/// Send the staged items to `to_ipk` as one album (or a lone message), with
/// `caption` on the first and `reply_to` quoting a prior message on all of
/// them. Fire-and-forget like the other sends: the `Result` reports invalid
/// input synchronously, outcomes arrive via `on_message`.
///
/// Every id must be ready. A caller that sends mid-encode should wait for the
/// buffer to settle rather than have items silently dropped.
#[uniffi::export]
pub fn send_staged(
    conversation_id: Vec<u8>, ids: Vec<u64>, caption: String, reply_to: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = to_conv16(&conversation_id)?;
    let reply = reply_to.as_deref().map(to_did16).transpose()?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::staging::commit(to, ids, caption, reply).await {
            log::error!("STAGING: commit failed: {e}");
        }
    });
    Ok(())
}

/// Replace a prior message's body with a staged item — the media half of an
/// edit. Refused by the compatibility matrix unless the swap is legal (an
/// attachment can only become another attachment, and so on); the item stays in
/// the buffer either way, so a refusal doesn't cost the user their pick.
#[uniffi::export]
pub fn revise_with_staged(
    conversation_id: Vec<u8>, dispatch_id: Vec<u8>, staged_id: u64, caption: String,
) -> Result<(), CoreError> {
    let to = to_conv16(&conversation_id)?;
    let target = to_did16(&dispatch_id)?;
    let body = crate::staging::body_of(staged_id, caption)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::revise(to, target, body).await {
            log::error!("STAGING: revise failed: {e}");
        }
    });
    Ok(())
}
