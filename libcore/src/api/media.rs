//! Inline-image send: FFI entry point for `send_image`.

use crate::api::messaging::to_did16;
use crate::api::messaging::to_fid32;
use crate::api::messaging::to_ipk32;
use crate::platform::CoreError;

#[derive(uniffi::Record)]
pub struct MediaRecord {
    pub dispatch_id: Vec<u8>,
    pub kind: u8,
    pub group_id: Option<Vec<u8>>,
    pub mime: String,
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub blob: Option<Vec<u8>>,
    pub thumb: Option<Vec<u8>>,
    pub file_id: Option<Vec<u8>>,
    pub transfer_state: u8,
    pub transfer_have: u32,
    pub transfer_total: u32,
    pub local_path: Option<String>,
}

/// Compress `rgba` to AVIF (≤256KB) and send it to `to_ipk` as an inline
/// `Image` message, with an optional `caption` and album `group_id`.
/// Fire-and-forget like [`crate::api::messaging::send_message`]: the
/// `Result` only reports invalid input synchronously, the send outcome
/// arrives via `on_message`.
#[uniffi::export]
pub fn send_image(
    to_ipk: Vec<u8>, rgba: Vec<u8>, width: u32, height: u32, caption: String,
    group_id: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = to_ipk32(&to_ipk)?;
    let gid = group_id.as_deref().map(to_did16).transpose()?;
    // Optimistic placeholder row FIRST — the DB change-hook doorbell pops the
    // bubble while the encode below blocks this FFI call.
    let msg = crate::messaging::build_image_message(to, width, height, &caption, gid)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .and_then(|d| d.try_into().ok())
        .expect("save_outgoing mints a dispatch_id");
    // compress_image stays SYNC on purpose — it surfaces the "can't meet the
    // 256KB inline budget → send as an attachment instead" error to the caller.
    // ponytail: on that failure the placeholder flashes then vanishes — rare
    // enough (over-budget photo) to live with.
    let (avif, w, h) = match crate::media::compress_image(&rgba, width, height, 256 * 1024) {
        Ok(v) => v,
        Err(e) => {
            let _ = crate::data::media::discard_outgoing(&to, &did);
            return Err(e.into());
        },
    };
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::finish_image(to, did, avif, w, h).await {
            log::warn!("MEDIA: send_image failed: {e}");
        }
    });
    Ok(())
}

/// Offer `source_path` to `to_ipk` as a P2P attachment: retain its manifest for
/// a week, blur an optional preview thumbnail, persist the caption + metadata
/// rows, and send the `Attachment` control (the bytes are pulled device-to-device
/// by `file_id`). Fire-and-forget like [`send_image`] — the `Result` reports only
/// synchronous input errors; the send outcome arrives via `on_message`.
#[uniffi::export]
pub fn send_attachment(
    to_ipk: Vec<u8>, source_path: String, name: String, mime: String,
    thumb_rgba: Option<Vec<u8>>, thumb_w: u32, thumb_h: u32, caption: String,
    group_id: Option<Vec<u8>>,
) -> Result<(), CoreError> {
    let to = to_ipk32(&to_ipk)?;
    let gid = group_id.as_deref().map(to_did16).transpose()?;
    let size = std::fs::metadata(&source_path)
        .map_err(|e| anyhow::anyhow!("stat {source_path}: {e}"))?
        .len();
    // No preview (zip/doc/audio) → None, stored as a NULL thumb so the UI's
    // "has preview?" check stays clean; the wire field flattens. Blur is light
    // next to the hash below, so it stays sync — the placeholder carries it.
    let thumb = thumb_rgba.map(|r| crate::media::blur_thumb(&r, thumb_w, thumb_h)).transpose()?;
    // Optimistic placeholder row FIRST — the bubble shows while prepare_send
    // (a BLAKE3 pass over the whole file, seconds for a big one) runs off-thread.
    let msg = crate::messaging::build_attachment_message(to, size, &name, &mime, thumb, &caption, gid)?;
    let did: [u8; 16] = msg
        .inner
        .dispatch_id
        .as_deref()
        .and_then(|d| d.try_into().ok())
        .expect("save_outgoing mints a dispatch_id");
    crate::RUNTIME.spawn(async move {
        // Only a prepare failure (file unreadable/gone) discards the placeholder —
        // the offer never existed. A send failure AFTER file_id is persisted (e.g.
        // we're offline) leaves the row pending for retry_pending_sends, never lost.
        let file_id = match crate::transfer::prepare_send(&source_path, 7 * 24 * 3600) {
            Ok((file_id, _size)) => file_id,
            Err(e) => {
                log::warn!("MEDIA: send_attachment prepare failed: {e}");
                let _ = crate::data::media::discard_outgoing(&to, &did);
                return;
            },
        };
        if let Err(e) = crate::messaging::finish_attachment(to, did, file_id).await {
            log::warn!("MEDIA: send_attachment send deferred to retry: {e}");
        }
    });
    Ok(())
}

/// Pull a received attachment's bytes by `file_id`. Fire-and-forget: dials the
/// sender (or reverse-wakes them if offline) and drives the resumable transfer;
/// progress and completion surface through `get_media`'s transfer_state.
#[uniffi::export]
pub fn download_attachment(file_id: Vec<u8>) -> Result<(), CoreError> {
    let fid = to_fid32(&file_id)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::transfer::download(fid).await {
            log::warn!("MEDIA: download_attachment failed: {e}");
        }
    });
    Ok(())
}

/// Read media records for a peer from the message_media table, with transfer
/// progress (state/have/total in chunks) joined in from the transfer store.
/// `local_path` is only exposed once the download is DONE — until then the
/// `.part` file holds unverified-tail bytes no platform should open.
#[uniffi::export]
pub fn get_media(peer_ipk: Vec<u8>) -> Result<Vec<MediaRecord>, CoreError> {
    use crate::transfer::store;
    let peer = to_ipk32(&peer_ipk)?;
    let rows = crate::data::media::for_peer(&peer)?;
    Ok(rows.into_iter().map(|(did, r)| {
        let fid = r.file_id.as_deref().and_then(|f| <&[u8; 32]>::try_from(f).ok());
        let (transfer_state, transfer_have, transfer_total, local_path) = match fid.and_then(store::partial_get) {
            Some(p) => (
                p.state,
                p.have,
                p.total.div_ceil(p.chunk_size.max(1) as u64) as u32,
                (p.state == store::DONE).then(|| p.path.clone()),
            ),
            // No receiver partial: this may be our OWN sent attachment, whose
            // file lives in `retention` under the same file_id. Surface it as a
            // complete local file so the sender can open what they sent.
            None => match fid.and_then(store::retention_get) {
                Some(ret) => {
                    let chunks = ret.size.div_ceil(ret.chunk_size.max(1) as u64) as u32;
                    (store::DONE, chunks, chunks, Some(ret.path))
                },
                None => (store::PENDING, 0, 0, None),
            },
        };
        MediaRecord {
            dispatch_id: did.to_vec(),
            kind: r.kind,
            group_id: r.group_id,
            mime: r.mime,
            name: r.name,
            size: r.size,
            width: r.width,
            height: r.height,
            blob: r.blob,
            thumb: r.thumb,
            file_id: r.file_id,
            transfer_state,
            transfer_have,
            transfer_total,
            local_path,
        }
    }).collect())
}

#[cfg(test)]
mod tests {
    use crate::data::media;

    /// Two-phase image persist: `build_image_message` lands the placeholder
    /// instantly (caption row + media row with a null blob, so the bubble can
    /// show), then `set_blob` fills the compressed bytes and final size.
    #[test]
    fn image_placeholder_persists_then_blob_fills() {
        let dir = std::env::temp_dir().join("promtuz-send-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [5u8; 32];
        let msg = crate::messaging::build_image_message(peer, 8, 8, "hi", None).unwrap();
        assert_eq!(msg.inner.content, "hi");
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();

        let rows = media::for_peer(&peer).unwrap();
        let (got_did, row) = rows.iter().find(|(d, _)| *d == did).expect("placeholder persisted");
        assert_eq!(*got_did, did);
        assert_eq!(row.kind, media::KIND_IMAGE);
        assert!(row.blob.is_none(), "placeholder carries no bytes yet");

        let rgba = vec![128u8; 8 * 8 * 4];
        let (avif, w, h) = crate::media::compress_image(&rgba, 8, 8, 256 * 1024).unwrap();
        assert!(!avif.is_empty());
        media::set_blob(&peer, &did, &avif, w, h).unwrap();

        let row = media::get(&peer, &did).unwrap().expect("media row");
        assert_eq!(row.blob.as_deref(), Some(avif.as_slice()));
        assert_eq!(row.size, avif.len() as u64);
    }

    /// Two-phase attachment persist: the placeholder lands with thumb, name
    /// and size but a null file_id (no manifest yet), then `set_file_id`
    /// finalizes it. Guards the thumb-as-`Option` contract.
    #[test]
    fn attachment_placeholder_persists_then_file_id_fills() {
        let dir = std::env::temp_dir().join("promtuz-send-attachment-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [9u8; 32];
        let thumb = vec![1u8, 2, 3];
        let msg = crate::messaging::build_attachment_message(
            peer, 4096, "doc.pdf", "application/pdf", Some(thumb.clone()), "here", None,
        )
        .unwrap();
        assert_eq!(msg.inner.content, "here");
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();

        let rows = media::for_peer(&peer).unwrap();
        let (_d, row) = rows.iter().find(|(d, _)| *d == did).expect("attachment media row persisted");
        assert_eq!(row.kind, media::KIND_ATTACHMENT);
        assert!(row.file_id.is_none(), "placeholder carries no file_id yet");
        assert_eq!(row.thumb, Some(thumb));
        assert_eq!(row.name, "doc.pdf");
        assert_eq!(row.size, 4096);
        assert!(row.blob.is_none());

        let file_id = [0xabu8; 32];
        media::set_file_id(&peer, &did, &file_id).unwrap();
        let row = media::get(&peer, &did).unwrap().expect("media row");
        assert_eq!(row.file_id.as_deref(), Some(file_id.as_slice()));
    }

    #[test]
    fn get_media_returns_media_records_for_peer() {
        let dir = std::env::temp_dir().join("promtuz-get-media-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [6u8; 32];
        let rgba = vec![128u8; 8 * 8 * 4];
        let (avif, w, h) = crate::media::compress_image(&rgba, 8, 8, 256 * 1024).unwrap();
        assert!(!avif.is_empty());

        let msg = crate::messaging::build_image_message(peer, w, h, "caption", None).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();
        media::set_blob(&peer, &did, &avif, w, h).unwrap();

        let records = super::get_media(peer.to_vec()).unwrap();
        let record = records.iter()
            .find(|r| r.dispatch_id == did.to_vec())
            .expect("media record found via FFI");
        assert_eq!(record.kind, media::KIND_IMAGE);
        assert!(record.blob.as_ref().is_some_and(|b| !b.is_empty()));
        assert_eq!(record.transfer_state, 0);
        assert_eq!(record.transfer_have, 0);
        assert_eq!(record.transfer_total, 0);
        assert_eq!(record.local_path, None);
    }

    /// The sender's own sent attachment has no receiver `partial` — its file
    /// lives in `retention` under the same file_id. `get_media` must surface it
    /// as DONE with the retained path so the user can open what they sent.
    #[test]
    fn get_media_surfaces_sender_own_retained_file() {
        let dir = std::env::temp_dir().join("promtuz-get-media-retain-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        use crate::transfer::store;
        let peer = [7u8; 32];
        let file_id = [0xcdu8; 32];
        let msg = crate::messaging::build_attachment_message(
            peer, 300 * 1024, "big.zip", "application/zip", None, "mine", None,
        )
        .unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();
        media::set_file_id(&peer, &did, &file_id).unwrap();
        store::retention_put(&file_id, "/tmp/big.zip", 300 * 1024, 256 * 1024, &[1, 2, 3], u64::MAX)
            .unwrap();

        let records = super::get_media(peer.to_vec()).unwrap();
        let record = records.iter().find(|r| r.dispatch_id == did.to_vec()).expect("record found");
        assert_eq!(record.transfer_state, store::DONE);
        assert_eq!(record.local_path.as_deref(), Some("/tmp/big.zip"));
        assert_eq!(record.transfer_total, 2); // 300KB / 256KB, div_ceil
        assert_eq!(record.transfer_have, record.transfer_total, "all chunks present");
    }
}
