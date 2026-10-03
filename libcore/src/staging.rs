//! Composer staging: media prepared off-thread before it becomes a message. The registry lives in
//! memory, so unsent media never reaches `messages`.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use std::sync::LazyLock;
use parking_lot::Mutex;

use crate::data::media::KIND_ATTACHMENT;
use crate::data::media::KIND_IMAGE;
use crate::data::media::MediaRow;
use crate::state::core;

pub const PREPARING: u8 = 0;
pub const READY: u8 = 1;
pub const FAILED: u8 = 2;

#[derive(Clone, Debug)]
pub struct Staged {
    pub id:      u64,
    /// `KIND_IMAGE` or `KIND_ATTACHMENT`.
    pub kind:    u8,
    pub state:   u8,
    pub mime:    String,
    pub name:    String,
    pub size:    u64,
    pub width:   u32,
    pub height:  u32,
    /// AVIF bytes of an image.
    pub blob:    Option<Vec<u8>>,
    pub thumb:   Option<Vec<u8>>,
    pub file_id: Option<[u8; 32]>,
    pub error:   Option<String>,
}

impl Staged {
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
            duration_ms: 0,
            sticker: None,
        })
    }
}

static ITEMS: LazyLock<Mutex<HashMap<u64, Staged>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Files a direct send retained and has not yet named on its message row, once per send.
static SENDING: Mutex<Vec<[u8; 32]>> = Mutex::new(Vec::new());

/// Keeps a direct send's file from cleanup until dropped, without naming it on the row a reconnect
/// retry would send.
pub(crate) struct Hold([u8; 32]);

impl Hold {
    pub(crate) fn new(file_id: [u8; 32]) -> Self {
        SENDING.lock().push(file_id);
        Self(file_id)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        let mut sending = SENDING.lock();
        if let Some(i) = sending.iter().position(|f| *f == self.0) {
            sending.swap_remove(i);
        }
    }
}

fn ring() {
    if let Some(events) = core().events.get() {
        events.on_db_changed(vec!["staging".to_string()]);
    }
}

pub fn list() -> Vec<Staged> {
    let mut v: Vec<Staged> = ITEMS.lock().values().cloned().collect();
    v.sort_by_key(|s| s.id);
    v
}

/// A prepare job still running lands through [`finish`], which ignores a discarded id.
pub fn discard(id: u64) {
    let orphans = {
        let mut items = ITEMS.lock();
        let gone = items.remove(&id);
        orphans_of(&items, gone.into_iter())
    };
    ring();
    release(&orphans);
}

pub fn clear() {
    let orphans = {
        let mut items = ITEMS.lock();
        let gone = std::mem::take(&mut *items);
        orphans_of(&items, gone.into_values())
    };
    ring();
    release(&orphans);
}

/// Consulted by the message-side unlink: a chip or a send in flight can hold the same file as a
/// deleted message.
pub(crate) fn holds(file_id: &[u8; 32]) -> bool {
    SENDING.lock().contains(file_id) || ITEMS.lock().values().any(|s| s.file_id == Some(*file_id))
}

/// Files `gone` held that no remaining item holds. Retention is one row per content hash, so the
/// same document under two chips is one file.
fn orphans_of(items: &HashMap<u64, Staged>, gone: impl Iterator<Item = Staged>) -> Vec<[u8; 32]> {
    gone.filter_map(|s| s.file_id)
        .filter(|f| !items.values().any(|s| s.file_id == Some(*f)))
        .collect()
}

/// Unlinks what unsent items retained; a just-committed item keeps its file through its new
/// `message_media` row. Takes the messages lock only after the `ITEMS` lock is released.
fn release(fids: &[[u8; 32]]) {
    if !fids.is_empty() {
        crate::data::media::unlink_orphaned(&core().db, &core().db.messages().lock(), fids);
    }
}

/// `false` when the item was discarded meanwhile, so the caller can release what it produced.
fn finish(id: u64, f: impl FnOnce(&mut Staged)) -> bool {
    let mut items = ITEMS.lock();
    let Some(s) = items.get_mut(&id) else { return false };
    f(s);
    drop(items);
    ring();
    true
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

pub fn stage_image(rgba: Vec<u8>, width: u32, height: u32) -> u64 {
    let mut s = blank(KIND_IMAGE);
    s.mime = "image/avif".into();
    s.width = width;
    s.height = height;
    let id = insert(s);

    core().spawn_blocking(move || {
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

pub fn stage_attachment(
    source_path: String, name: String, mime: String, thumb_rgba: Option<Vec<u8>>, thumb_w: u32,
    thumb_h: u32,
) -> Result<u64> {
    let size = std::fs::metadata(&source_path)
        .map_err(|e| anyhow!("stat {source_path}: {e}"))?
        .len();
    let thumb = thumb_rgba.map(|r| crate::media::attachment_thumb(&mime, &r, thumb_w, thumb_h)).transpose()?;

    let mut s = blank(KIND_ATTACHMENT);
    s.mime = mime;
    s.name = name;
    s.size = size;
    s.thumb = thumb;
    let id = insert(s);

    let path = source_path.clone();
    core().spawn_blocking(move || {
        match crate::transfer::prepare_send(&path, 7 * 24 * 3600, |file_id| hold(id, file_id)) {
            Ok((file_id, _size)) => {
                let landed = finish(id, |s| {
                    s.file_id = Some(file_id);
                    s.state = READY;
                });
                // Discarded while the hash ran: the retention it just wrote is an orphan.
                if !landed {
                    let ghost = Staged { file_id: Some(file_id), ..blank(KIND_ATTACHMENT) };
                    let orphans = orphans_of(&ITEMS.lock(), std::iter::once(ghost));
                    release(&orphans);
                }
            },
            Err(e) => {
                finish(id, |s| {
                    s.state = FAILED;
                    s.error = Some(e.to_string());
                });
            },
        }
    });
    Ok(id)
}

/// Names the chip's file before it is retained, so an unlink of the same content meanwhile keeps
/// the new copy.
fn hold(id: u64, file_id: [u8; 32]) {
    if let Some(s) = ITEMS.lock().get_mut(&id) {
        s.file_id = Some(file_id);
    }
}

/// Items leave the buffer as they commit, so a partial failure does not re-send what already went.
/// The caption rides the first item and `reply_to` rides all of them.
pub async fn commit(
    conversation: [u8; 16], ids: Vec<u64>, caption: String, reply_to: Option<[u8; 16]>,
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

    // A lone item gets no group id, so it renders as a single photo, not a one-member album.
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
        let msg =
            crate::data::media::save_outgoing_with_media(&conversation, cap, reply_to, &media)?;
        discard(s.id);
        let payload = crate::messaging::body::rebuild_pending_payload(&conversation, &msg)?;
        // The durable message owns it now; a network failure must not stop the rest of the album.
        core().spawn(async move {
            if let Err(e) = crate::messaging::send_prepared(conversation, &msg, payload).await {
                log::debug!("STAGING: message remains pending: {e}");
            }
        });
    }
    Ok(())
}

/// For a revise: leaves the item in the buffer for the caller to commit or discard.
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
    use std::path::Path;

    use super::*;
    use crate::test_support::ScopedCore;

    /// Retention is one row per content hash, so two chips from one document share a file.
    #[test]
    fn a_file_stays_held_while_any_chip_still_names_it() {
        let file = [9u8; 32];
        let chip = || Staged { file_id: Some(file), state: READY, ..blank(KIND_ATTACHMENT) };
        let (a, b) = (chip(), chip());
        let mut items = HashMap::from([(a.id, a.clone()), (b.id, b.clone())]);

        let gone = items.remove(&a.id);
        assert!(orphans_of(&items, gone.into_iter()).is_empty(), "the other chip still names it");
        let gone = items.remove(&b.id);
        assert_eq!(orphans_of(&items, gone.into_iter()), vec![file]);
    }

    /// A chip names its file before the copy is retained, so deleting the same content elsewhere
    /// in the meantime keeps the chip's copy, which only its own discard frees.
    #[tokio::test]
    async fn a_chip_holds_its_file_from_before_it_is_retained() {
        let scope = ScopedCore::new();
        let db = &scope.core.db;
        let picked = format!("{}/picked.bin", db.files_dir("picked"));
        std::fs::write(&picked, [5u8; 2048]).unwrap();
        let id = insert(blank(KIND_ATTACHMENT));
        let (file_id, _) = crate::transfer::prepare_send(&picked, 60, |f| hold(id, f)).unwrap();
        let retained = || {
            let transfers = db.transfers().lock();
            crate::transfer::store::retention_get_tx(&transfers, &file_id).map(|r| r.path)
        };

        // A message with the same content is deleted before the chip lands.
        crate::data::media::unlink_orphaned(db, &db.messages().lock(), &[file_id]);
        let kept = retained().expect("the chip keeps its retention");
        assert!(Path::new(&kept).exists());
        discard(id);
        assert_eq!(retained(), None);
        assert!(!Path::new(&kept).exists());
    }
}
