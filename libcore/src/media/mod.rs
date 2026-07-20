//! Still-image pipeline: RGBA (from the platform decoder) → AVIF, plus a
//! gaussian-blurred thumbnail. libcore owns encode/blur so it's one impl for
//! every platform; the platform owns decode (HEIC/HDR/EXIF) + video/PDF.

use anyhow::{bail, Result};
use ravif::{Encoder, Img, RGBA8};

/// Encode RGBA pixels to AVIF, downscaling/re-quantizing until the output
/// fits `max_bytes` (256KB for inline `Image`). Returns `(avif_bytes, out_w, out_h)`.
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

    let mut w = width;
    let mut h = height;
    let mut buf: Vec<RGBA8> = rgba
        .chunks_exact(4)
        .map(|p| RGBA8::new(p[0], p[1], p[2], p[3]))
        .collect();

    // Quality/scale ladder: drop quality first, then halve dimensions, until
    // the encode fits the budget.
    for scale_step in 0..4 {
        if scale_step > 0 {
            let img = image::RgbaImage::from_raw(
                w,
                h,
                buf.iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect(),
            )
            .expect("buf matches w*h*4");
            let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
            let small = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
            w = small.width();
            h = small.height();
            buf = small
                .pixels()
                .map(|p| RGBA8::new(p[0], p[1], p[2], p[3]))
                .collect();
        }
        for &q in &[70.0f32, 55.0, 40.0, 28.0] {
            let out = Encoder::new()
                .with_quality(q)
                .with_speed(6)
                .encode_rgba(Img::new(buf.as_slice(), w as usize, h as usize))?;
            if out.avif_file.len() <= max_bytes {
                return Ok((out.avif_file, w, h));
            }
        }
    }

    // Nothing on the ladder fit. This buffer is inlined into a hard-capped MLS
    // `Image` frame, so returning an over-budget buffer would be a silent
    // contract break — error instead. An image too large to inline is meant to
    // route to the P2P `Attachment` path; the caller propagates this Err.
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

    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec()).unwrap();

    // Downscale to ≤48px longest side, preserving aspect ratio.
    let scale = 48.0 / width.max(height) as f32;
    let (tw, th) = (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    );

    let small = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Triangle);
    let blurred = image::imageops::blur(&small, 2.0);

    // Convert pixels to RGBA8 for encoding.
    let px: Vec<RGBA8> = blurred
        .pixels()
        .map(|p| RGBA8::new(p[0], p[1], p[2], p[3]))
        .collect();

    // Encode with modest quality.
    let out = Encoder::new()
        .with_quality(50.0)
        .with_speed(8)
        .encode_rgba(Img::new(px.as_slice(), tw as usize, th as usize))?;

    Ok(out.avif_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(w: u32, h: u32) -> Vec<u8> {
        vec![128u8; (w * h * 4) as usize]
    }

    // High-entropy input: AVIF can't crush it to a trivial size, so the
    // quality/scale ladder is actually forced to run (solid gray fits at q70
    // on the first try and never exercises the ladder).
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

    // Tight budget on noisy input that a full-res encode can't meet: the ladder
    // must downscale to fit, and the output must actually be <= budget (I1).
    // 256x256 noise is ~72KB at q70; a 15KB budget can only be met after at
    // least one halving.
    #[test]
    fn compress_engages_ladder_under_tight_budget() {
        let (w, h) = (256, 256);
        let budget = 15 * 1000;
        let (bytes, ow, oh) = compress_image(&noisy_rgba(w, h), w, h, budget).unwrap();
        assert!(is_avif(&bytes));
        assert!(bytes.len() <= budget, "over budget: {}", bytes.len());
        assert!(ow < w || oh < h, "ladder never downscaled: {ow}x{oh}");
    }

    // Impossibly tight budget: no ladder rung (down to 32x32) can fit under
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
