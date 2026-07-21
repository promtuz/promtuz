//! Still-image pipeline: RGBA (from the platform decoder) → AVIF, plus a
//! gaussian-blurred thumbnail. libcore owns encode/blur so it's one impl for
//! every platform; the platform owns decode (HEIC/HDR/EXIF) + video/PDF.

use anyhow::{bail, Result};
use ravif::{Encoder, Img};
use rgb::FromSlice;

fn encode_avif(rgba: &[u8], w: u32, h: u32, quality: f32) -> Result<Vec<u8>> {
    Ok(Encoder::new()
        .with_quality(quality)
        .with_speed(8)
        .encode_rgba(Img::new(rgba.as_rgba(), w as usize, h as usize))?
        .avif_file)
}

/// Encode RGBA pixels to AVIF under `max_bytes` (256KB for inline `Image`).
/// Encodes once at full size; on overshoot, downscales proportionally to the
/// overshoot ratio and retries. Returns `(avif_bytes, out_w, out_h)`.
pub fn compress_image(
    rgba: &[u8],
    width: u32,
    height: u32,
    max_bytes: usize,
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

    // AVIF bytes scale ~linearly with pixel count at fixed quality, so the
    // overshoot ratio gives the target dimensions directly (0.85 = headroom).
    // Each retry resizes from the ORIGINAL buffer so resampling loss never
    // cascades.
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

    // The buffer inlines into a hard-capped MLS `Image` frame, so returning an
    // over-budget buffer would be a silent contract break — error instead. An
    // image too large to inline is meant to route to the P2P `Attachment`
    // path; the caller propagates this Err.
    bail!("could not compress under {max_bytes} bytes");
}

/// Downscale RGBA to ≤48px longest side, gaussian blur, then AVIF-encode.
/// Returns a tiny blurred thumbnail (a few KB).
pub fn blur_thumb(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        bail!("zero dimension");
    }
    if rgba.len() != (width as usize * height as usize * 4) {
        bail!("rgba len mismatch");
    }

    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width, height, rgba)
        .expect("len checked above");

    // Downscale to ≤48px longest side, preserving aspect ratio.
    let scale = 48.0 / width.max(height) as f32;
    let (tw, th) = (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    );

    let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::blur(&small, 2.0);

    encode_avif(blurred.as_raw(), tw, th, 50.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(w: u32, h: u32) -> Vec<u8> {
        vec![128u8; (w * h * 4) as usize]
    }

    // High-entropy input: AVIF can't crush it to a trivial size, so the
    // downscale retry is actually forced to run (solid gray fits on the first
    // full-size encode and never exercises it).
    fn noisy_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        let mut s: u32 = 0x9E37_79B9;
        for _ in 0..(w * h) {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            v.extend_from_slice(&[(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8, 255]);
        }
        v
    }

    // ISOBMFF: bytes[4..8] is the `ftyp` box type; the `avif` brand sits after.
    fn is_avif(b: &[u8]) -> bool {
        b.len() > 8 && &b[4..8] == b"ftyp" && b.windows(4).any(|c| c == b"avif")
    }

    #[test]
    fn compress_produces_valid_bounded_avif() {
        let (w, h) = (640, 480);
        let (bytes, ow, oh) = compress_image(&solid_rgba(w, h), w, h, 256 * 1024).unwrap();
        assert!(is_avif(&bytes));
        assert!(bytes.len() <= 256 * 1024, "over budget: {}", bytes.len());
        assert!(ow <= w && oh <= h);
    }

    // Tight budget on noisy input that a full-res encode can't meet: the retry
    // must downscale to fit, and the output must actually be <= budget (I1).
    #[test]
    fn compress_engages_ladder_under_tight_budget() {
        let (w, h) = (256, 256);
        let budget = 15 * 1000;
        let (bytes, ow, oh) = compress_image(&noisy_rgba(w, h), w, h, budget).unwrap();
        assert!(is_avif(&bytes));
        assert!(bytes.len() <= budget, "over budget: {}", bytes.len());
        assert!(ow < w || oh < h, "ladder never downscaled: {ow}x{oh}");
    }

    // Impossibly tight budget: nothing (down to the 64px floor) fits under
    // 100 bytes of AVIF container overhead, so compress must Err rather than
    // silently hand back an over-budget buffer (I1).
    #[test]
    fn compress_bails_when_budget_impossible() {
        let (w, h) = (256, 256);
        assert!(compress_image(&noisy_rgba(w, h), w, h, 100).is_err());
    }

    #[test]
    fn compress_rejects_zero_dimension() {
        assert!(compress_image(&[], 0, 0, 256 * 1024).is_err());
    }

    #[test]
    fn blur_thumb_is_small_valid_avif() {
        let (w, h) = (640, 480);
        let out = blur_thumb(&solid_rgba(w, h), w, h).unwrap();
        assert!(is_avif(&out), "not a valid avif");
        assert!(out.len() < 8 * 1024, "thumb too big: {}", out.len());
    }

    #[test]
    fn blur_thumb_rejects_zero_dimension() {
        assert!(blur_thumb(&[], 0, 0).is_err());
    }
}
