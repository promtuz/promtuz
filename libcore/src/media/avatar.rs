//! Avatar preparation uses the same source-preserving media pipeline, with the
//! existing wire budget. Persistence accepts the reviewed AVIF bytes unchanged.
use anyhow::{Result, ensure};
use common::proto::mls_wire::MAX_AVATAR_BYTES;

use super::{AVATAR_EDGE, MAX_ENCODED_SOURCE_BYTES, MediaPolicy, PreparedImage, gif, inspect_avif};

/// Source-relative square crop, independent of the preview's dimensions.
#[derive(Clone, Copy)]
pub struct AvatarCrop {
    pub center_x: f64,
    pub center_y: f64,
    pub zoom: f64,
}

#[derive(Clone, Copy)]
pub(super) struct CropArea {
    pub x: u32,
    pub y: u32,
    pub side: u32,
}

impl AvatarCrop {
    fn validate(self) -> Result<()> {
        ensure!(
            self.center_x.is_finite()
                && self.center_y.is_finite()
                && self.zoom.is_finite()
                && (0.0..=1.0).contains(&self.center_x)
                && (0.0..=1.0).contains(&self.center_y)
                && (1.0..=5.0).contains(&self.zoom),
            "The picture crop is invalid"
        );
        Ok(())
    }

    pub(super) fn area(self, width: u32, height: u32) -> Result<CropArea> {
        self.validate()?;
        ensure!(width > 0 && height > 0, "The picture has no pixels");
        // GIF pixels are discrete. Round the editor's source-relative square
        // once, then apply the same bounds to each fully composited frame.
        let side = (f64::from(width.min(height)) / self.zoom).round().max(1.0) as u32;
        let x = (self.center_x * f64::from(width) - f64::from(side) / 2.0)
            .round()
            .clamp(0.0, f64::from(width - side)) as u32;
        let y = (self.center_y * f64::from(height) - f64::from(side) / 2.0)
            .round()
            .clamp(0.0, f64::from(height - side)) as u32;
        Ok(CropArea { x, y, side })
    }
}

/// A GIF without a crop is a fitted preview; with a crop, every composited frame
/// is cropped before encoding. AVIF remains byte-for-byte identical, so only a
/// true no-op square crop can be requested for that format.
pub fn prepare_avatar_image(bytes: &[u8], crop: Option<AvatarCrop>) -> Result<PreparedImage> {
    ensure!(bytes.len() <= MAX_ENCODED_SOURCE_BYTES, "Image exceeds the 32 MiB import limit");
    if let Some(crop) = crop {
        crop.validate()?;
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let policy = MediaPolicy { max_bytes: MAX_AVATAR_BYTES, max_edge: AVATAR_EDGE };
        return if let Some(crop) = crop {
            gif::prepare_cropped(bytes, policy, crop)
        } else {
            gif::prepare(bytes, policy)
        };
    }

    prepare_avif(bytes, crop)
}

/// Validate already-prepared AVIF before any profile/group write. Deliberately
/// does not accept or convert GIF: saving must preserve the previewed payload.
pub fn validate_avatar_avif(bytes: &[u8]) -> Result<PreparedImage> {
    prepare_avif(bytes, None)
}

fn prepare_avif(bytes: &[u8], crop: Option<AvatarCrop>) -> Result<PreparedImage> {
    ensure!(
        bytes.len() <= MAX_AVATAR_BYTES,
        "This picture exceeds the 64 KiB profile picture limit. Choose a smaller AVIF or a GIF that can be resized"
    );
    let info = inspect_avif(bytes)?.ok_or_else(|| {
        anyhow::anyhow!("Profile pictures must be prepared as AVIF before saving")
    })?;
    ensure!(
        info.width.max(info.height) <= AVATAR_EDGE && info.max_coded_edge <= AVATAR_EDGE,
        "This AVIF exceeds the 256 pixel profile picture limit. Its animation and colour data cannot be resized safely yet"
    );
    if let Some(crop) = crop {
        ensure!(
            crop.center_x == 0.5
                && crop.center_y == 0.5
                && crop.zoom == 1.0
                && info.width == info.height
                && info.rotation_quarter_turns == 0
                && info.mirror_axis.is_none(),
            "This AVIF can be kept as the original, but cannot be cropped without changing its animation or colour data"
        );
    }
    Ok(PreparedImage {
        bytes: bytes.to_vec(),
        mime: "image/avif",
        width: info.width,
        height: info.height,
        animated: info.animated,
    })
}
