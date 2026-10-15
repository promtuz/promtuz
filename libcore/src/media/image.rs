use std::io::Cursor;

use ::image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use anyhow::{Result, bail, ensure};

use super::{
    frames::{Frame, PixelFormat},
    policy::MediaRequest,
};

pub struct DecodedAvif {
    pub frame: Frame,
    pub color: promtuz_avif::ColorInfo,
    pub source_depth: u32,
    pub frame_count: u32,
    pub has_alpha: bool,
    pub has_gain_map: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Avif,
    Jpeg,
    Png,
    Gif,
    WebP,
    Platform,
}

pub struct DecodedStill {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Content classification is deliberately independent of picker MIME types.
pub fn inspect(bytes: &[u8]) -> Result<ImageKind> {
    if crate::media::inspect_avif(bytes)?.is_some() {
        return Ok(ImageKind::Avif);
    }
    match ::image::guess_format(bytes) {
        Ok(ImageFormat::Jpeg) => Ok(ImageKind::Jpeg),
        Ok(ImageFormat::Png) => Ok(ImageKind::Png),
        Ok(ImageFormat::Gif) => Ok(ImageKind::Gif),
        Ok(ImageFormat::WebP) => Ok(ImageKind::WebP),
        Ok(_) => Ok(ImageKind::Platform),
        Err(_) => bail!("This image format is not supported."),
    }
}

/// Decodes a core-owned SDR still with orientation baked into pixels. Animated
/// sources take the frame path instead, so this can never silently discard them.
pub fn decode_sdr_still(bytes: &[u8]) -> Result<DecodedStill> {
    let kind = inspect(bytes)?;
    ensure!(
        matches!(kind, ImageKind::Jpeg | ImageKind::Png),
        "This image needs animated or platform decoding."
    );
    let format = match kind {
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Png => ImageFormat::Png,
        _ => unreachable!(),
    };
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    Ok(DecodedStill { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    let limits = MediaRequest::resource_limits();
    ensure!(width > 0 && height > 0, "The image has invalid dimensions.");
    ensure!(
        u64::from(width) * u64::from(height) <= limits.source_pixels,
        "The image exceeds the 32 megapixel import limit."
    );
    Ok(())
}

pub fn decode_avif(bytes: &[u8]) -> Result<DecodedAvif> {
    let limits = MediaRequest::resource_limits();
    let image = promtuz_avif::decode_first(
        bytes,
        promtuz_avif::DecodeLimits {
            max_pixels: u32::try_from(limits.source_pixels)?,
            max_edge: 8_192,
            max_frames: limits.animation_frames,
        },
    )
    .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
    let pixels = image.rgba16().iter().flat_map(|pixel| pixel.to_ne_bytes()).collect();
    Ok(DecodedAvif {
        frame: Frame::new(pixels, image.width, image.height, PixelFormat::Rgba16, 0)?,
        color: image.color,
        source_depth: image.source_depth,
        frame_count: image.frame_count,
        has_alpha: image.has_alpha,
        has_gain_map: image.has_gain_map,
    })
}

#[cfg(test)]
mod tests {
    use ::image::{ColorType, ImageEncoder};

    use super::*;

    #[test]
    fn avif_decode_preserves_16_bit_frame_shape_and_color_description() {
        let encoded =
            super::super::encode_avif(&[255, 0, 0, 255, 0, 255, 0, 255], 2, 1, 60.0).unwrap();
        let decoded = decode_avif(&encoded).unwrap();
        assert_eq!((decoded.frame.width, decoded.frame.height), (2, 1));
        assert_eq!(decoded.frame.format, PixelFormat::Rgba16);
        assert_eq!(decoded.frame.pixels.len(), 16);
        assert_eq!(decoded.source_depth, 8);
        assert_eq!(decoded.frame_count, 1);
    }

    #[test]
    fn core_classifies_and_decodes_a_still_without_picker_metadata() {
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[1, 2, 3, 4], 1, 1, ColorType::Rgba8.into())
            .unwrap();
        assert_eq!(inspect(&png).unwrap(), ImageKind::Png);
        let decoded = decode_sdr_still(&png).unwrap();
        assert_eq!((decoded.width, decoded.height, decoded.rgba), (1, 1, vec![1, 2, 3, 4]));
    }
}
