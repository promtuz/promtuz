//! Composer staging: media sitting in the buffer before it becomes a message.
//!
//! Picking a photo used to *be* the send. Staging splits that in two:
//! [`stage_image`] / [`stage_attachment`] return an id immediately and run the
//! expensive pass (AVIF compress, BLAKE3 manifest) off-thread, so it overlaps
//! with the caption still being typed; [`commit`] turns the ready items into
//! messages once the user actually sends.
//!
//! The registry is in memory: a staged item is exactly as transient as the text
//! draft beside it, and holding it here keeps un-sent media out of `messages`
//! entirely — no draft can leak into history, a conversation preview, or the
//! pending-send retry sweep. An attachment's manifest is the exception and
//! lives on in the transfer store, so the costly hash survives even when the
//! staging entry doesn't.
//!
//! State changes ring the [`crate::platform::CoreEvents::on_db_changed`]
//! doorbell under `"staging"`, so the client re-reads through the same reactive
//! path it uses for every other table.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::data::media::KIND_ATTACHMENT;
use crate::data::media::KIND_IMAGE;
use crate::data::media::MediaRow;

/// The expensive pass is still running; the UI draws a progress ring.
pub const PREPARING: u8 = 0;
/// Encoded and ready to ride a send.
pub const READY: u8 = 1;
/// The pass failed; [`Staged::error`] says why and the item is user-removable.
pub const FAILED: u8 = 2;

/// One item in the composer buffer — everything a `Body` needs, minus the
/// message it will eventually ride.
#[derive(Clone, Debug)]
pub struct Staged {
    pub id:      u64,
    /// `KIND_IMAGE` / `KIND_ATTACHMENT`, matching the media side-row kinds.
    pub kind:    u8,
    pub state:   u8,
    pub mime:    String,
    pub name:    String,
    pub size:    u64,
    pub width:   u32,
    pub height:  u32,
    /// AVIF bytes for an image, once the compress lands.
    pub blob:    Option<Vec<u8>>,
    /// Blurred preview for an attachment; present from the moment it's staged.
    pub thumb:   Option<Vec<u8>>,
    /// Content-addressed id for an attachment, once the manifest pass lands.
    pub file_id: Option<[u8; 32]>,
    pub error:   Option<String>,
}

impl Staged {
    /// The media side-row this item becomes. `None` while it's still preparing
    /// or has failed — [`commit`] refuses those rather than sending a husk.
    fn media_row(&self, group_id: Option<[u8; 16]>) -> Option<MediaRow> {
        if self.state != READY {
            return None;
        }
        Some(MediaRow {
            kind:     self.kind,
            group_id: group_id.map(|g| g.to_vec()),
            mime:     self.mime.clone(),
            name:     self.name.clone(),
            size:     self.size,
            width:    self.width,
            height:   self.height,
            blob:     self.blob.clone(),
            thumb:    self.thumb.clone(),
            file_id:  self.file_id.map(|f| f.to_vec()),
        })
    }
}

static ITEMS: Lazy<Mutex<HashMap<u64, Staged>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Ring the client's re-read doorbell for the staging buffer.
fn ring() {
    if let Some(events) = crate::platform::EVENTS.get() {
        events.on_db_changed(vec!["staging".to_string()]);
    }
}

/// Everything currently in the buffer, oldest first — the id order items were
/// staged in, which is the order they send in.
pub fn list() -> Vec<Staged> {
    let mut v: Vec<Staged> = ITEMS.lock().values().cloned().collect();
    v.sort_by_key(|s| s.id);
    v
}

/// Drop one item. Its prepare job may still be running — the completion writes
/// through [`finish`], which no-ops on an id that's gone, so a removal mid-pass
/// can't resurrect it.
pub fn discard(id: u64) {
    ITEMS.lock().remove(&id);
    ring();
}

/// Empty the buffer (send committed, or the composer was cleared).
pub fn clear() {
    ITEMS.lock().clear();
    ring();
}

/// Land a finished prepare onto its item, unless it was discarded meanwhile.
fn finish(id: u64, f: impl FnOnce(&mut Staged)) {
    let mut items = ITEMS.lock();
    if let Some(s) = items.get_mut(&id) {
        f(s);
        drop(items);
        ring();
    }
}

fn insert(s: Staged) -> u64 {
    let id = s.id;
    ITEMS.lock().insert(id, s);
    ring();
    id
}

fn blank(kind: u8) -> Staged {
    Staged {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        state: PREPARING,
        mime: String::new(),
        name: String::new(),
        size: 0,
        width: 0,
        height: 0,
        blob: None,
        thumb: None,
        file_id: None,
        error: None,
    }
}

/// Stage a picked photo. Returns before the compress runs, so the thumbnail can
/// appear at once; the AVIF pass lands through [`finish`] and flips the item to
/// [`READY`]. Over-budget photos fail here rather than at send time, which is
/// the whole point of encoding while it sits in the buffer.
pub fn stage_image(rgba: Vec<u8>, width: u32, height: u32) -> u64 {
    let mut s = blank(KIND_IMAGE);
    s.mime = "image/avif".into();
    s.width = width;
    s.height = height;
    let id = insert(s);

    crate::RUNTIME.spawn_blocking(move || {
        match crate::media::compress_image(&rgba, width, height, 256 * 1024) {
            Ok((avif, w, h)) => finish(id, |s| {
                s.size = avif.len() as u64;
                s.blob = Some(avif);
                s.width = w;
                s.height = h;
                s.state = READY;
            }),
            Err(e) => finish(id, |s| {
                s.state = FAILED;
                s.error = Some(e.to_string());
            }),
        }
    });
    id
}

/// Stage a picked file. The blurred preview is computed synchronously (it's
/// light next to the hash) so the buffer has something to draw immediately; the
/// manifest pass runs off-thread and lands the `file_id`.
pub fn stage_attachment(
    source_path: String, name: String, mime: String, thumb_rgba: Option<Vec<u8>>, thumb_w: u32,
    thumb_h: u32,
) -> Result<u64> {
    let size = std::fs::metadata(&source_path)
        .map_err(|e| anyhow!("stat {source_path}: {e}"))?
        .len();
    let thumb = thumb_rgba.map(|r| crate::media::blur_thumb(&r, thumb_w, thumb_h)).transpose()?;

    let mut s = blank(KIND_ATTACHMENT);
    s.mime = mime;
    s.name = name;
    s.size = size;
    s.thumb = thumb;
    let id = insert(s);

    let path = source_path.clone();
    crate::RUNTIME.spawn_blocking(move || {
        match crate::transfer::prepare_send(&path, 7 * 24 * 3600) {
            Ok((file_id, _size)) => finish(id, |s| {
                s.file_id = Some(file_id);
                s.state = READY;
            }),
            Err(e) => finish(id, |s| {
                s.state = FAILED;
                s.error = Some(e.to_string());
            }),
        }
    });
    Ok(id)
}

/// Turn the staged items into messages and send them.
///
/// Every id must be [`READY`] — a caller that sends mid-encode should await the
/// doorbell rather than have this quietly drop a husk. Several items share one
/// album `group_id` and the caption rides the first, matching how a multi-pick
/// has always grouped; `reply_to` rides all of them, which is what makes a
/// quoted photo possible at all.
///
/// Items leave the buffer as they commit, so a partial failure doesn't re-send
/// what already went.
pub async fn commit(
    to: [u8; 32], ids: Vec<u64>, caption: String, reply_to: Option<[u8; 16]>,
) -> Result<()> {
    if ids.is_empty() {
        bail!("nothing staged");
    }
    let staged: Vec<Staged> = {
        let items = ITEMS.lock();
        ids.iter()
            .map(|id| items.get(id).cloned().ok_or_else(|| anyhow!("staged item {id} is gone")))
            .collect::<Result<_>>()?
    };
    if let Some(s) = staged.iter().find(|s| s.state != READY) {
        bail!("staged item {} is not ready ({})", s.id, s.error.as_deref().unwrap_or("preparing"));
    }

    // An album needs a shared id; a lone item carries none, so a single photo
    // keeps rendering as a single photo rather than a one-member group.
    let group_id: Option<[u8; 16]> = (staged.len() > 1).then(|| {
        use ed25519_dalek::ed25519::signature::rand_core::OsRng;
        use ed25519_dalek::ed25519::signature::rand_core::RngCore;

        let mut g = [0u8; 16];
        OsRng.fill_bytes(&mut g);
        g
    });

    for (i, s) in staged.iter().enumerate() {
        let media = s.media_row(group_id).ok_or_else(|| anyhow!("staged item {} not ready", s.id))?;
        let cap = if i == 0 { caption.as_str() } else { "" };
        let msg = crate::data::media::save_outgoing_with_media(&to, cap, reply_to, &media)?;
        discard(s.id);
        let payload = crate::messaging::rebuild_pending_payload(&to, &msg)?;
        crate::messaging::send_prepared(to, &msg, payload).await?;
    }
    Ok(())
}

/// Seed a ready item without running a prepare pass — the async jobs need a
/// runtime and real encoders, which the registry's own behaviour doesn't.
#[cfg(test)]
fn insert_ready(kind: u8) -> u64 {
    let mut s = blank(kind);
    s.state = READY;
    s.mime = "image/avif".into();
    s.width = 2;
    s.height = 2;
    s.size = 3;
    match kind {
        KIND_IMAGE => s.blob = Some(vec![1, 2, 3]),
        _ => {
            s.file_id = Some([9u8; 32]);
            s.thumb = Some(vec![4, 5]);
        },
    }
    insert(s)
}

/// The body a staged item would send as, for a revise rather than a new
/// message. Leaves the item in the buffer — the caller commits or discards.
pub fn body_of(id: u64, caption: String) -> Result<common::proto::mls_wire::Body> {
    use common::proto::mls_wire::Body;

    let items = ITEMS.lock();
    let s = items.get(&id).ok_or_else(|| anyhow!("staged item {id} is gone"))?;
    if s.state != READY {
        bail!("staged item {id} is not ready");
    }
    Ok(match s.kind {
        KIND_IMAGE => Body::Image {
            caption,
            group_id: None,
            mime: s.mime.clone(),
            width: s.width,
            height: s.height,
            data: s.blob.clone().ok_or_else(|| anyhow!("ready image with no bytes"))?,
        },
        KIND_ATTACHMENT => Body::Attachment {
            caption,
            group_id: None,
            mime: s.mime.clone(),
            name: s.name.clone(),
            size: s.size,
            thumb: s.thumb.clone().unwrap_or_default(),
            file_id: s.file_id.ok_or_else(|| anyhow!("ready attachment with no file_id"))?,
        },
        k => bail!("staged item {id} has unknown kind {k}"),
    })
}

#[cfg(test)]
mod tests {
    use common::proto::mls_wire::Body;

    use super::*;

    /// The registry is process-global, so a test that asserts on `list()` has to
    /// own it for the duration.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn items_list_in_staging_order_and_leave_on_discard() {
        let _g = SERIAL.lock();
        clear();

        let a = insert_ready(KIND_IMAGE);
        let b = insert_ready(KIND_ATTACHMENT);
        assert_eq!(list().iter().map(|s| s.id).collect::<Vec<_>>(), vec![a, b]);

        discard(a);
        assert_eq!(list().iter().map(|s| s.id).collect::<Vec<_>>(), vec![b]);

        clear();
        assert!(list().is_empty());
    }

    /// A prepare landing after the user removed the item must not resurrect it —
    /// otherwise a discarded photo reappears, ready, at send time.
    #[test]
    fn a_prepare_that_lands_after_a_discard_is_dropped() {
        let _g = SERIAL.lock();
        clear();

        let id = insert_ready(KIND_IMAGE);
        discard(id);
        finish(id, |s| s.state = READY);
        assert!(list().is_empty(), "finish must no-op on a discarded id");
    }

    #[test]
    fn body_of_projects_each_kind_and_refuses_what_is_not_ready() {
        let _g = SERIAL.lock();
        clear();

        let img = insert_ready(KIND_IMAGE);
        assert!(matches!(
            body_of(img, "cap".into()).unwrap(),
            Body::Image { caption, data, .. } if caption == "cap" && data == vec![1, 2, 3]
        ));

        let att = insert_ready(KIND_ATTACHMENT);
        assert!(matches!(
            body_of(att, String::new()).unwrap(),
            Body::Attachment { file_id, .. } if file_id == [9u8; 32]
        ));

        // Still encoding: a body now would be a husk with no bytes.
        finish(img, |s| s.state = PREPARING);
        assert!(body_of(img, String::new()).is_err(), "a preparing item has no body");

        assert!(body_of(u64::MAX, String::new()).is_err(), "a gone item has no body");
        clear();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commit_refuses_an_empty_or_unready_buffer() {
        let _g = SERIAL.lock();
        clear();

        let to = [0x60u8; 32];
        assert!(commit(to, vec![], String::new(), None).await.is_err(), "nothing to send");

        let id = insert_ready(KIND_IMAGE);
        finish(id, |s| s.state = PREPARING);
        assert!(
            commit(to, vec![id], String::new(), None).await.is_err(),
            "a mid-encode item must block the send rather than go as a husk",
        );
        clear();
    }
}
