//! Source-aware image processing shared by photos, stickers and avatars.

mod avatar;
mod avif;
pub mod frames;
#[path = "image.rs"]
pub mod image;
pub mod import;
mod job;
mod policy;
pub mod ports;
pub(crate) mod video;

pub use avatar::validate_avatar_avif;
pub use avif::{AvifInfo, inspect_avif};
pub use job::{MediaJob, PreparedMedia, StagedImage};
pub use policy::{MediaRequest, NormalizedCrop, TimeRange};

use crate::state::core;
use anyhow::Result;

pub(crate) fn alert(alert: crate::platform::MediaAlert) {
    if let Some(events) = core().events.get() {
        events.on_media_alert(alert);
    }
}

fn encode_avif(rgba: &[u8], w: u32, h: u32, quality: f32) -> Result<Vec<u8>> {
    image::encode_sdr_avif(rgba, w, h, quality)
}

const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
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

fn validate_rgba(rgba: &[u8], width: u32, height: u32) -> Result<()> {
    validate_dimensions(width, height)?;
    anyhow::ensure!(
        u64::from(width) * u64::from(height) * 4 == rgba.len() as u64,
        "rgba len mismatch"
    );
    Ok(())
}

/// A blurred placeholder of a few KB, 48 px on the longest side.
pub fn blur_thumb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    validate_rgba(rgba, width, height)?;

    let img = ::image::ImageBuffer::<::image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");

    let scale = 48.0 / width.max(height) as f32;
    let (tw, th) = (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    );

    let small = ::image::imageops::resize(&img, tw, th, ::image::imageops::FilterType::Triangle);
    let blurred = ::image::imageops::blur(&small, 2.0);

    encode_avif(blurred.as_raw(), tw, th, 50.0)
}

/// Longest side of a picture or video poster sent ahead of a P2P attachment.
pub const POSTER_EDGE: u32 = 320;
const MAX_POSTER_BYTES: usize = 32 * 1024;

/// A sharp preview for a picture or video attachment, drawn as a tile before the bytes arrive.
pub fn poster_thumb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    validate_rgba(rgba, width, height)?;
    let img = ::image::ImageBuffer::<::image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");
    let mut edge = POSTER_EDGE.min(width.max(height));
    loop {
        let scale = edge as f32 / width.max(height) as f32;
        let (tw, th) = (
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
        );
        let small =
            ::image::imageops::resize(&img, tw, th, ::image::imageops::FilterType::Triangle);
        let out = encode_avif(small.as_raw(), tw, th, 50.0)?;
        if out.len() <= MAX_POSTER_BYTES || edge <= 96 {
            return Ok(out);
        }
        edge = (edge * 3 / 4).max(96);
    }
}

/// A sharp poster for media, a blur for everything else.
pub fn attachment_thumb(mime: &str, rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if mime.starts_with("image/") || mime.starts_with("video/") {
        poster_thumb(rgba, width, height)
    } else {
        blur_thumb(rgba, width, height)
    }
}
