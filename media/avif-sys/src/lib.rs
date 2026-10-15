use std::ffi::c_int;

#[repr(C)]
pub struct DecodedImage {
    pub pixels: *mut u8,
    pub pixel_bytes: usize,
    pub width: u32,
    pub height: u32,
    pub source_depth: u32,
    pub frame_count: u32,
    pub duration_ms: u32,
    pub color_primaries: u16,
    pub transfer_characteristics: u16,
    pub matrix_coefficients: u16,
    pub full_range: bool,
    pub has_alpha: bool,
    pub has_gain_map: bool,
}

#[repr(C)]
pub struct ImageInfo {
    pub width: u32,
    pub height: u32,
    pub source_depth: u32,
    pub frame_count: u32,
    pub color_primaries: u16,
    pub transfer_characteristics: u16,
    pub matrix_coefficients: u16,
    pub full_range: bool,
    pub has_alpha: bool,
    pub has_gain_map: bool,
}

#[repr(C)]
pub struct Decoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    pub fn promtuz_avif_decode_rgba16(
        input: *const u8, input_len: usize, max_pixels: u32, max_edge: u32, max_frames: u32,
        output: *mut DecodedImage,
    ) -> c_int;
    pub fn promtuz_avif_decoder_open(
        input: *const u8, input_len: usize, max_pixels: u32, max_edge: u32, max_frames: u32,
        info: *mut ImageInfo, output: *mut *mut Decoder,
    ) -> c_int;
    pub fn promtuz_avif_decoder_next(decoder: *mut Decoder, output: *mut DecodedImage) -> c_int;
    pub fn promtuz_avif_decoder_destroy(decoder: *mut Decoder);
    pub fn promtuz_avif_free_pixels(pixels: *mut u8);
}

pub const LIMIT_EXCEEDED: c_int = -1;
pub const NO_IMAGES_REMAINING: c_int = 16;
