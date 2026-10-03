//! Source-aware image processing shared by photos, stickers and avatars.
//!
//! Encoded AVIF bypasses pixel conversion so its animation, precision, colour
//! profile and gain map remain intact. GIF is composited and encoded as an AVIF
//! sequence. Platform-decoded RGBA is explicitly an eight-bit SDR input.

mod avif;
mod avatar;
mod gif;
mod sequence;

pub use avif::{AvifInfo, inspect_avif};
pub use avatar::{AvatarCrop, prepare_avatar_image, validate_avatar_avif};

use anyhow::{Result, bail};
use common::proto::mls_wire::MAX_AVATAR_BYTES;
use ravif::{BitDepth, Encoder, Img};
use rgb::FromSlice;

fn encode_avif(rgba: &[u8], w: u32, h: u32, quality: f32) -> Result<Vec<u8>> {
    Ok(Encoder::new()
        .with_quality(quality)
        .with_speed(8)
        .with_bit_depth(BitDepth::Eight)
        .encode_rgba(Img::new(rgba.as_rgba(), w as usize, h as usize))?
        .avif_file)
}

/// Processing limits belong to the caller's media use, not the input format.
#[derive(Clone, Copy)]
pub struct MediaPolicy {
    pub max_bytes: usize,
    pub max_edge: u32,
}

pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

pub const MAX_ENCODED_SOURCE_BYTES: usize = 32 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 16_384;

/// `None` means another format must be handled by its platform decoder. A
/// recognized malformed or over-limit source is an error, never a still fallback.
pub fn process_encoded_image(bytes: &[u8], policy: MediaPolicy) -> Result<Option<PreparedImage>> {
    anyhow::ensure!(policy.max_bytes > 0 && policy.max_edge > 0, "invalid media limits");
    anyhow::ensure!(
        bytes.len() <= MAX_ENCODED_SOURCE_BYTES,
        "Image exceeds the 32 MiB import limit"
    );
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return gif::prepare(bytes, policy).map(Some);
    }
    let Some(info) = inspect_avif(bytes)? else { return Ok(None) };
    anyhow::ensure!(
        info.width.max(info.height) <= policy.max_edge && info.max_coded_edge <= policy.max_edge,
        "This AVIF exceeds the {} pixel limit. Its animation and colour data cannot be resized safely yet",
        policy.max_edge
    );
    anyhow::ensure!(
        bytes.len() <= policy.max_bytes,
        "This AVIF exceeds the {} byte limit. Its animation and colour data cannot be recompressed safely yet",
        policy.max_bytes
    );
    Ok(Some(PreparedImage {
        bytes: bytes.to_vec(),
        mime: "image/avif",
        width: info.width,
        height: info.height,
        animated: info.animated,
    }))
}

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

fn fit_dimensions(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let scale = (f64::from(edge) / f64::from(width.max(height))).min(1.0);
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

/// SDR still stickers use the same precision rules as photos and GIF frames.
pub fn compress_sticker(rgba: &[u8], width: u32, height: u32) -> Result<(Vec<u8>, u32, u32)> {
    validate_rgba(rgba, width, height)?;
    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("validated RGBA");
    let mut edge = 512;
    for quality in [78.0, 62.0, 48.0] {
        let (w, h) = fit_dimensions(width, height, edge);
        let scaled = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
        let out = Encoder::new()
            .with_quality(quality)
            .with_alpha_quality(85.0)
            .with_speed(7)
            .with_bit_depth(BitDepth::Eight)
            .encode_rgba(Img::new(scaled.as_raw().as_rgba(), w as usize, h as usize))?
            .avif_file;
        if out.len() <= 256 * 1024 {
            return Ok((out, w, h));
        }
        edge = (edge * 3 / 4).max(128);
    }
    bail!("Picture would not fit a sticker")
}

/// Returns `(avif, width, height)` under `max_bytes`, downscaling when the full size overshoots.
pub fn compress_image(
    rgba: &[u8], width: u32, height: u32, max_bytes: usize,
) -> Result<(Vec<u8>, u32, u32)> {
    validate_rgba(rgba, width, height)?;

    let mut out = encode_avif(rgba, width, height, 60.0)?;
    if out.len() <= max_bytes {
        return Ok((out, width, height));
    }

    // AVIF size scales about linearly with pixel count, so the overshoot ratio gives the
    // target dimensions; each retry resizes the original so resampling loss does not cascade.
    let orig = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");
    let (mut w, mut h) = (width, height);
    for _ in 0..2 {
        let s = (0.85 * max_bytes as f64 / out.len() as f64).sqrt();
        w = ((w as f64 * s).max(64.0).min(width as f64)) as u32;
        h = ((h as f64 * s).max(64.0).min(height as f64)) as u32;
        let small = image::imageops::resize(&orig, w, h, image::imageops::FilterType::Triangle);
        out = encode_avif(small.as_raw(), w, h, 60.0)?;
        if out.len() <= max_bytes {
            return Ok((out, w, h));
        }
    }

    // The result inlines into a capped MLS `Image` frame; an over-budget buffer must be an error.
    bail!("could not compress under {max_bytes} bytes");
}

/// A blurred placeholder of a few KB, 48 px on the longest side.
pub fn blur_thumb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    validate_rgba(rgba, width, height)?;

    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");

    let scale = 48.0 / width.max(height) as f32;
    let (tw, th) = (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    );

    let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::blur(&small, 2.0);

    encode_avif(blurred.as_raw(), tw, th, 50.0)
}

/// Longest side of a picture or video poster sent ahead of a P2P attachment.
pub const POSTER_EDGE: u32 = 320;
const MAX_POSTER_BYTES: usize = 32 * 1024;

/// A sharp preview for a picture or video attachment, drawn as a tile before the bytes arrive.
pub fn poster_thumb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    validate_rgba(rgba, width, height)?;
    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");
    let mut edge = POSTER_EDGE.min(width.max(height));
    loop {
        let scale = edge as f32 / width.max(height) as f32;
        let (tw, th) = (
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
        );
        let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
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

/// Longest side of a profile picture; it travels inside an MLS frame to every chat we are in.
pub const AVATAR_EDGE: u32 = 256;

/// Centre-crop to a square of at most [`AVATAR_EDGE`] and encode under [`MAX_AVATAR_BYTES`];
/// quality steps down before size, and a picture that fits at no quality is refused.
pub fn avatar_from_rgba(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    validate_rgba(rgba, width, height)?;
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).expect("len checked above");

    let side = width.min(height);
    let square =
        image::imageops::crop_imm(&img, (width - side) / 2, (height - side) / 2, side, side)
            .to_image();
    let edge = side.min(AVATAR_EDGE);
    let scaled = if edge == side {
        square
    } else {
        image::imageops::resize(&square, edge, edge, image::imageops::FilterType::Lanczos3)
    };

    for quality in [65.0, 45.0, 30.0] {
        let out = encode_avif(scaled.as_raw(), edge, edge, quality)?;
        if out.len() <= MAX_AVATAR_BYTES {
            return Ok(out);
        }
    }
    bail!("could not encode the picture under {MAX_AVATAR_BYTES} bytes");
}
