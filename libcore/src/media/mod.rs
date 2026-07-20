//! Still-image pipeline: RGBA (from the platform decoder) → AVIF, plus a
//! gaussian-blurred thumbnail. libcore owns encode/blur so it's one impl for
//! every platform; the platform owns decode (HEIC/HDR/EXIF) + video/PDF.
//!
//! `blur_thumb` lands in Task 5.

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

    // Last resort: smallest/fastest attempt even if still over budget —
    // caller decides whether to reject.
    let out = Encoder::new()
        .with_quality(28.0)
        .with_speed(8)
        .encode_rgba(Img::new(buf.as_slice(), w as usize, h as usize))?;
    Ok((out.avif_file, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(w: u32, h: u32) -> Vec<u8> {
        vec![128u8; (w * h * 4) as usize]
    }

    #[test]
    fn compress_produces_valid_bounded_avif() {
        let (w, h) = (640, 480);
        let (bytes, ow, oh) = compress_image(&solid_rgba(w, h), w, h, 256 * 1024).unwrap();
        assert!(!bytes.is_empty());
        assert!(bytes.len() <= 256 * 1024, "over budget: {}", bytes.len());
        // ISOBMFF: bytes[4..8] is the `ftyp` box type; the brand (`avif`/`avis`)
        // sits right after it.
        assert_eq!(&bytes[4..8], b"ftyp");
        assert!(bytes.windows(4).any(|c| c == b"avif"));
        assert!(ow <= w && oh <= h);
    }
}
