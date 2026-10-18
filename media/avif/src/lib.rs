use std::{ffi::CString, mem::MaybeUninit, path::Path, ptr::NonNull, slice};

// `libavif` resolves rav1e's C ABI at the final link. Referencing one ABI symbol
// keeps rav1e's C-API object in the Rust link without exposing it to callers.
#[used]
static RAV1E_C_API: unsafe extern "C" fn() -> *const std::ffi::c_char =
    rav1e::capi::rav1e_version_short;

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
    _input: Option<Vec<u8>>,
    pub info: ImageInfo,
}

// libavif's decoder state has no thread-affine resources. Callers still need
// exclusive access for every decode operation; the public FFI session enforces
// that with a mutex.
unsafe impl Send for Decoder {}

pub struct Encoder {
    raw: NonNull<promtuz_avif_sys::Encoder>,
}

#[derive(Debug, Clone, Copy)]
pub struct EncodeConfig {
    pub quantizer: u8,
    pub speed: u8,
    pub timescale: u32,
    pub animated: bool,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self { quantizer: 46, speed: 8, timescale: 1_000, animated: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    Encoder(i32),
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
        let input = input.to_vec();
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
        Ok(Self { raw, _input: Some(input), info: image_info(info) })
    }

    pub fn open_file(path: &Path, limits: DecodeLimits) -> Result<Self, DecodeError> {
        let path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| DecodeError::Decoder(-2))?;
        let mut raw = std::ptr::null_mut();
        let mut info = MaybeUninit::<promtuz_avif_sys::ImageInfo>::zeroed();
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_decoder_open_file(
                path.as_ptr(),
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
        Ok(Self { raw, _input: None, info: image_info(unsafe { info.assume_init() }) })
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

impl Encoder {
    pub fn new(width: u32, height: u32, config: EncodeConfig) -> Result<Self, EncodeError> {
        let mut raw = std::ptr::null_mut();
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_encoder_create(
                width,
                height,
                u32::from(config.quantizer),
                i32::from(config.speed),
                config.timescale,
                config.animated,
                &mut raw,
            )
        };
        if result != 0 {
            return Err(EncodeError::Encoder(result));
        }
        Ok(Self { raw: NonNull::new(raw).ok_or(EncodeError::Encoder(-2))? })
    }

    pub fn add_rgba(&mut self, pixels: &[u8], duration: u32) -> Result<(), EncodeError> {
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_encoder_add_rgba(
                self.raw.as_ptr(),
                pixels.as_ptr(),
                pixels.len(),
                duration,
            )
        };
        if result == 0 { Ok(()) } else { Err(EncodeError::Encoder(result)) }
    }

    pub fn finish(self) -> Result<Vec<u8>, EncodeError> {
        let mut bytes = std::ptr::null_mut();
        let mut size = 0;
        let result = unsafe {
            promtuz_avif_sys::promtuz_avif_encoder_finish(self.raw.as_ptr(), &mut bytes, &mut size)
        };
        if result != 0 {
            return Err(EncodeError::Encoder(result));
        }
        let bytes = NonNull::new(bytes).ok_or(EncodeError::Encoder(-2))?;
        Ok(unsafe {
            let output = slice::from_raw_parts(bytes.as_ptr(), size).to_vec();
            promtuz_avif_sys::promtuz_avif_free_bytes(bytes.as_ptr());
            output
        })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { promtuz_avif_sys::promtuz_avif_encoder_destroy(self.raw.as_ptr()) };
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
    use std::io::Write;

    use super::*;
    #[test]
    fn rejects_malformed_data() {
        assert!(matches!(
            decode_first(b"not an avif", DecodeLimits::default()),
            Err(DecodeError::Decoder(_))
        ));
    }

    #[test]
    fn enforces_decode_limits_before_allocating_pixels() {
        let mut encoder = Encoder::new(2, 1, EncodeConfig::default()).unwrap();
        encoder.add_rgba(&[255, 0, 0, 255, 0, 255, 0, 255], 1).unwrap();
        let encoded = encoder.finish().unwrap();
        assert!(matches!(
            decode_first(&encoded, DecodeLimits { max_pixels: 1, ..DecodeLimits::default() }),
            Err(DecodeError::LimitExceeded)
        ));
    }

    #[test]
    fn opens_a_private_file_without_loading_the_container_in_rust() {
        let mut encoder = Encoder::new(1, 1, EncodeConfig::default()).unwrap();
        encoder.add_rgba(&[255, 0, 0, 255], 1).unwrap();
        let encoded = encoder.finish().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.avif");
        std::fs::File::create(&path).unwrap().write_all(&encoded).unwrap();
        let mut decoder = Decoder::open_file(&path, DecodeLimits::default()).unwrap();
        assert_eq!(decoder.decode_next().unwrap().unwrap().rgba16().len(), 4);
    }

    #[test]
    fn writes_a_still_with_the_pinned_libavif_encoder() {
        let mut encoder = Encoder::new(2, 1, EncodeConfig::default()).unwrap();
        encoder.add_rgba(&[255, 0, 0, 255, 0, 255, 0, 128], 1).unwrap();
        let encoded = encoder.finish().unwrap();
        let decoded = decode_first(&encoded, DecodeLimits::default()).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert!(decoded.has_alpha);
    }

    #[test]
    fn writes_an_avif_sequence_with_the_pinned_libavif_encoder() {
        let mut encoder =
            Encoder::new(16, 16, EncodeConfig { animated: true, ..EncodeConfig::default() })
                .unwrap();
        encoder.add_rgba(&[255, 0, 0, 255].repeat(16 * 16), 100).unwrap();
        encoder.add_rgba(&[0, 255, 0, 255].repeat(16 * 16), 100).unwrap();
        let encoded = encoder.finish().unwrap();
        let mut decoder = Decoder::open(&encoded, DecodeLimits::default()).unwrap();
        assert_eq!(decoder.info.frame_count, 2);
        assert_eq!(decoder.decode_next().unwrap().unwrap().duration_ms, 100);
        assert_eq!(decoder.decode_next().unwrap().unwrap().duration_ms, 100);
        assert!(decoder.decode_next().unwrap().is_none());
    }
}
