//! Inline-image send: FFI entry point for `send_image`.

use crate::api::messaging::to_did16;
use crate::api::messaging::to_ipk32;
use crate::platform::CoreError;

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
}
