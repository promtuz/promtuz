use std::{mem::MaybeUninit, ptr::NonNull, slice};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    LimitExceeded,
    Decoder(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
    pub full_range: bool,
}

pub struct Image {
    pixels: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub source_depth: u32,
    pub frame_count: u32,
    pub duration_ms: u32,
    pub color: ColorInfo,
    pub has_alpha: bool,
    pub has_gain_map: bool,
}

pub struct Decoder {
    raw: NonNull<promtuz_avif_sys::Decoder>,
    pub info: ImageInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub source_depth: u32,
    pub frame_count: u32,
    pub color: ColorInfo,
    pub has_alpha: bool,
    pub has_gain_map: bool,
}

impl Image {
    pub fn rgba16(&self) -> &[u16] {
        &self.pixels
    }
}

pub fn decode_first(input: &[u8], limits: DecodeLimits) -> Result<Image, DecodeError> {
    let mut decoder = Decoder::open(input, limits)?;
    decoder.decode_next()?.ok_or(DecodeError::Decoder(promtuz_avif_sys::NO_IMAGES_REMAINING))
}

impl Decoder {
    pub fn open(input: &[u8], limits: DecodeLimits) -> Result<Self, DecodeError> {
        let mut raw = std::ptr::null_mut();
        let mut info = MaybeUninit::<promtuz_avif_sys::ImageInfo>::zeroed();
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_decoder_open(
                input.as_ptr(),
                input.len(),
                limits.max_pixels,
                limits.max_edge,
                limits.max_frames,
                info.as_mut_ptr(),
                &mut raw,
            )
        };
        if result == promtuz_avif_sys::LIMIT_EXCEEDED {
            return Err(DecodeError::LimitExceeded);
        }
        if result != 0 {
            return Err(DecodeError::Decoder(result));
        }
        let raw = NonNull::new(raw).ok_or(DecodeError::Decoder(-2))?;
        let info = unsafe { info.assume_init() };
        Ok(Self { raw, info: image_info(info) })
    }

    pub fn decode_next(&mut self) -> Result<Option<Image>, DecodeError> {
        let mut raw = MaybeUninit::<promtuz_avif_sys::DecodedImage>::zeroed();
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_decoder_next(self.raw.as_ptr(), raw.as_mut_ptr())
        };
        if result == promtuz_avif_sys::NO_IMAGES_REMAINING {
            return Ok(None);
        }
        if result != 0 {
            return Err(DecodeError::Decoder(result));
        }
        let raw = unsafe { raw.assume_init() };
        Ok(Some(copy_image(raw)?))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { promtuz_avif_sys::promtuz_avif_decoder_destroy(self.raw.as_ptr()) };
    }
}

fn image_info(raw: promtuz_avif_sys::ImageInfo) -> ImageInfo {
    ImageInfo {
        width: raw.width,
        height: raw.height,
        source_depth: raw.source_depth,
        frame_count: raw.frame_count,
        color: ColorInfo {
            primaries: raw.color_primaries,
            transfer: raw.transfer_characteristics,
            matrix: raw.matrix_coefficients,
            full_range: raw.full_range,
        },
        has_alpha: raw.has_alpha,
        has_gain_map: raw.has_gain_map,
    }
}

fn copy_image(raw: promtuz_avif_sys::DecodedImage) -> Result<Image, DecodeError> {
    let expected = usize::try_from(raw.width)
        .ok()
        .and_then(|width| {
            usize::try_from(raw.height).ok().and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|channels| channels.checked_mul(std::mem::size_of::<u16>()));
    if raw.pixels.is_null() || expected != Some(raw.pixel_bytes) {
        unsafe { promtuz_avif_sys::promtuz_avif_free_pixels(raw.pixels) };
        return Err(DecodeError::Decoder(-2));
    }
    let pixels = unsafe {
        let slice = slice::from_raw_parts(raw.pixels.cast::<u16>(), raw.pixel_bytes / 2);
        let copy = slice.to_vec();
        promtuz_avif_sys::promtuz_avif_free_pixels(raw.pixels);
        copy
    };
    Ok(Image {
        pixels,
        width: raw.width,
        height: raw.height,
        source_depth: raw.source_depth,
        frame_count: raw.frame_count,
        duration_ms: raw.duration_ms,
        color: ColorInfo {
            primaries: raw.color_primaries,
            transfer: raw.transfer_characteristics,
            matrix: raw.matrix_coefficients,
            full_range: raw.full_range,
        },
        has_alpha: raw.has_alpha,
        has_gain_map: raw.has_gain_map,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_pixels: u32,
    pub max_edge: u32,
    pub max_frames: u32,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self { max_pixels: 32_000_000, max_edge: 8_192, max_frames: 600 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ravif::{Encoder, Img};
    use rgb::FromSlice;

    #[test]
    fn decodes_ravif_output() {
        let pixels = [255_u8, 0, 0, 255, 0, 255, 0, 128];
        let encoded =
            Encoder::new().encode_rgba(Img::new(pixels.as_rgba(), 2, 1)).unwrap().avif_file;
        let decoded = decode_first(&encoded, DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.rgba16().len(), 8);
        assert!(decoded.has_alpha);
    }

    #[test]
    fn rejects_malformed_data() {
        assert!(matches!(
            decode_first(b"not an avif", DecodeLimits::default()),
            Err(DecodeError::Decoder(_))
        ));
    }

    #[test]
    fn enforces_decode_limits_before_allocating_pixels() {
        let pixels = [255_u8, 0, 0, 255, 0, 255, 0, 255];
        let encoded =
            Encoder::new().encode_rgba(Img::new(pixels.as_rgba(), 2, 1)).unwrap().avif_file;
        assert!(matches!(
            decode_first(&encoded, DecodeLimits { max_pixels: 1, ..DecodeLimits::default() }),
            Err(DecodeError::LimitExceeded)
        ));
    }
}
