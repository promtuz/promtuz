//! Inline-image send: FFI entry point for `send_image`.

use crate::api::messaging::to_did16;
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
    let (avif, w, h) = crate::media::compress_image(&rgba, width, height, 256 * 1024)?;
    crate::RUNTIME.spawn(async move {
        if let Err(e) = crate::messaging::send_image(to, avif, w, h, caption, gid).await {
            log::warn!("MEDIA: send_image failed: {e}");
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
        let partial = r.file_id.as_deref()
            .and_then(|f| <&[u8; 32]>::try_from(f).ok())
            .and_then(store::partial_get);
        let (transfer_state, transfer_have, transfer_total, local_path) = match partial {
            Some(p) => (
                p.state,
                p.have,
                p.total.div_ceil(p.chunk_size.max(1) as u64) as u32,
                (p.state == store::DONE).then(|| p.path.clone()),
            ),
            None => (0, 0, 0, None),
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

    /// `build_image_message` compresses (via the caller, here a solid RGBA
    /// square already compressed) + persists both rows in one pass: a
    /// `messages` row with the caption, and a `message_media` row carrying
    /// the AVIF blob. Mirrors `data::media`'s own read-back test.
    #[test]
    fn image_prep_compresses_and_persists_media_row() {
        let dir = std::env::temp_dir().join("promtuz-send-image-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024

        let peer = [5u8; 32];
        let rgba = vec![128u8; 8 * 8 * 4];
        let (avif, w, h) = crate::media::compress_image(&rgba, 8, 8, 256 * 1024).unwrap();
        assert!(!avif.is_empty());

        let msg = crate::messaging::build_image_message(peer, &avif, w, h, "hi", None).unwrap();
        assert_eq!(msg.inner.content, "hi");
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();

        let rows = media::for_peer(&peer).unwrap();
        let (got_did, row) = rows.iter().find(|(d, _)| *d == did).expect("media row persisted");
        assert_eq!(*got_did, did);
        assert_eq!(row.kind, media::KIND_IMAGE);
        assert!(row.blob.as_deref().is_some_and(|b| !b.is_empty()));
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

        let msg = crate::messaging::build_image_message(peer, &avif, w, h, "caption", None).unwrap();
        let did: [u8; 16] = msg.inner.dispatch_id.clone().unwrap().try_into().unwrap();

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
}
