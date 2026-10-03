//! RGBA from the platform decoder to AVIF, plus the thumbnails that ride the wire.

use anyhow::{bail, Result};
use common::proto::mls_wire::MAX_AVATAR_BYTES;
use ravif::{Encoder, Img};
use rgb::FromSlice;

fn encode_avif(rgba: &[u8], w: u32, h: u32, quality: f32) -> Result<Vec<u8>> {
    Ok(Encoder::new()
        .with_quality(quality)
        .with_speed(8)
        .encode_rgba(Img::new(rgba.as_rgba(), w as usize, h as usize))?
        .avif_file)
}

/// Returns `(avif, width, height)` under `max_bytes`, downscaling when the full size overshoots.
pub fn compress_image(
    rgba: &[u8], width: u32, height: u32, max_bytes: usize,
) -> Result<(Vec<u8>, u32, u32)> {
    if width == 0 || height == 0 {
        bail!("zero dimension");
    }
    if rgba.len() != width as usize * height as usize * 4 {
        bail!("rgba len mismatch");
    }

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
    if width == 0 || height == 0 {
        bail!("zero dimension");
    }
    if rgba.len() != (width as usize * height as usize * 4) {
        bail!("rgba len mismatch");
    }

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
    if width == 0 || height == 0 {
        bail!("zero dimension");
    }
    if rgba.len() != (width as usize * height as usize * 4) {
        bail!("rgba len mismatch");
    }
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
    if width == 0 || height == 0 {
        bail!("zero dimension");
    }
    if rgba.len() != width as usize * height as usize * 4 {
        bail!("rgba len mismatch");
    }
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
