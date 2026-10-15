use anyhow::Result;

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
}
