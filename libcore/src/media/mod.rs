//! Source-aware image processing shared by photos, stickers and avatars.

mod avif;
pub mod frames;
#[path = "image.rs"]
pub mod image;
pub mod import;
mod job;
mod policy;
pub mod ports;
pub(crate) mod video;

pub use avif::{AvifInfo, inspect_avif};
pub use job::{MediaJob, PreparedMedia, StagedImage, StagedVideo};
pub use policy::{MediaRequest, NormalizedCrop, TimeRange};

use crate::state::core;
use anyhow::Result;

pub(crate) fn alert(alert: crate::platform::MediaAlert) {
    if let Some(events) = core().events.get() {
        events.on_media_alert(alert);
    }
}

#[cfg(test)]
fn encode_avif(rgba: &[u8], w: u32, h: u32, quality: f32) -> Result<Vec<u8>> {
    image::encode_sdr_avif(rgba, w, h, quality)
}

const MAX_IMAGE_PIXELS: u64 = 32_000_000;
const MAX_IMAGE_EDGE: u32 = 16_384;

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    anyhow::ensure!(width > 0 && height > 0, "zero image dimension");
    anyhow::ensure!(
        width <= MAX_IMAGE_EDGE
            && height <= MAX_IMAGE_EDGE
            && u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
        "Image dimensions exceed the decoding limit"
    );
    Ok(())
}
