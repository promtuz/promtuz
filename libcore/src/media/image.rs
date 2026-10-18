use std::{
    fs::File,
    io::{BufReader, Cursor, Read},
    path::Path,
    sync::OnceLock,
};

use ::image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use anyhow::{Result, bail, ensure};

use super::{
    frames::{Frame, FrameStore, PixelFormat},
    policy::{ImageProfile, MediaRequest, NormalizedCrop},
    ports::{ImageFrameRequest, SourceLease},
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

pub struct PreparedStill {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    pub animation_fallback: bool,
}

pub struct DecodedFrames {
    pub width: u32,
    pub height: u32,
    pub frames: FrameStore,
}

/// Content classification is deliberately independent of picker MIME types.
pub fn inspect(bytes: &[u8]) -> Result<ImageKind> {
    if is_jxl(bytes) {
        return Ok(ImageKind::Platform);
    }
    if is_isobmff(bytes) {
        return Ok(if has_avif_brand(bytes) { ImageKind::Avif } else { ImageKind::Platform });
    }
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

/// Formats core deliberately delegates to an image adapter. This excludes
/// generic ISO-BMFF containers so an audio-only MP4 can remain a raw file.
pub fn platform_image_candidate(header: &[u8]) -> bool {
    is_jxl(header) || is_heif(header)
}

/// The private import is the only file path media decoding accepts. Picker URIs
/// never reach this layer, and ordinary formats stream from that private copy.
pub fn inspect_path(path: &Path) -> Result<ImageKind> {
    let mut header = [0_u8; 64];
    let used = File::open(path)?.read(&mut header)?;
    let header = &header[..used];
    if is_jxl(header) || is_isobmff(header) {
        return Ok(if has_avif_brand(header) { ImageKind::Avif } else { ImageKind::Platform });
    }
    match ::image::guess_format(header) {
        Ok(ImageFormat::Jpeg) => Ok(ImageKind::Jpeg),
        Ok(ImageFormat::Png) => Ok(ImageKind::Png),
        Ok(ImageFormat::Gif) => Ok(ImageKind::Gif),
        Ok(ImageFormat::WebP) => Ok(ImageKind::WebP),
        Ok(_) => Ok(ImageKind::Platform),
        Err(_) => bail!("This image format is not supported."),
    }
}

fn is_isobmff(header: &[u8]) -> bool {
    header.len() >= 16 && &header[4..8] == b"ftyp"
}

fn is_jxl(header: &[u8]) -> bool {
    header.starts_with(&[0xff, 0x0a])
        || header.starts_with(&[0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a])
}

fn is_heif(header: &[u8]) -> bool {
    is_isobmff(header)
        && header[8..].chunks_exact(4).any(|brand| {
            matches!(brand, b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" | b"mif1" | b"msf1")
        })
}

fn has_avif_brand(header: &[u8]) -> bool {
    header[8..].chunks_exact(4).any(|brand| matches!(brand, b"avif" | b"avis"))
}

/// Decodes a core-owned SDR still with orientation baked into pixels. Animated
/// sources take the frame path instead, so this can never silently discard them.
pub fn decode_sdr_still(bytes: &[u8]) -> Result<DecodedStill> {
    let kind = inspect(bytes)?;
    decode_sdr_still_reader(kind, Cursor::new(bytes))
}

pub fn decode_sdr_still_path(path: &Path) -> Result<DecodedStill> {
    let kind = inspect_path(path)?;
    decode_sdr_still_reader(kind, BufReader::new(File::open(path)?))
}

fn decode_sdr_still_reader(
    reader_kind: ImageKind, reader: impl std::io::BufRead + std::io::Seek,
) -> Result<DecodedStill> {
    ensure!(
        matches!(reader_kind, ImageKind::Jpeg | ImageKind::Png),
        "This image needs animated or platform decoding."
    );
    let format = match reader_kind {
        ImageKind::Jpeg => ImageFormat::Jpeg,
        ImageKind::Png => ImageFormat::Png,
        _ => unreachable!(),
    };
    let mut decoder = ImageReader::with_format(reader, format).into_decoder()?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    Ok(DecodedStill { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

pub fn decode_animation_path(path: &Path) -> Result<DecodedFrames> {
    let kind = inspect_path(path)?;
    match kind {
        ImageKind::Gif => collect_frames(
            ::image::codecs::gif::GifDecoder::new(BufReader::new(File::open(path)?))?.into_frames(),
        ),
        ImageKind::Png => {
            let decoder = ::image::codecs::png::PngDecoder::new(BufReader::new(File::open(path)?))?;
            ensure!(decoder.is_apng()?, "The PNG is not animated.");
            collect_frames(decoder.apng()?.into_frames())
        },
        ImageKind::WebP => collect_frames(
            ::image::codecs::webp::WebPDecoder::new(BufReader::new(File::open(path)?))?
                .into_frames(),
        ),
        _ => bail!("This image format does not use the core animation decoder."),
    }
}

/// The common SDR image path. Every successful result is a freshly encoded
/// AVIF with no source container metadata.
pub fn prepare_sdr_image(bytes: &[u8], profile: ImageProfile) -> Result<PreparedImage> {
    prepare_sdr_image_with_crop(bytes, profile, None)
}

pub fn prepare_sdr_image_with_crop(
    bytes: &[u8], profile: ImageProfile, crop: Option<NormalizedCrop>,
) -> Result<PreparedImage> {
    let kind = inspect(bytes)?;
    let (still, animation_fallback) = match kind {
        ImageKind::Jpeg => (decode_sdr_still(bytes)?, false),
        ImageKind::Png => {
            let png = ::image::codecs::png::PngDecoder::new(Cursor::new(bytes))?;
            if png.is_apng()? {
                let mut frames = decode_animation(bytes)?.frames;
                let frame = frames.frame(0)?;
                (
                    DecodedStill { rgba: frame.pixels, width: frame.width, height: frame.height },
                    true,
                )
            } else {
                (decode_sdr_still(bytes)?, false)
            }
        },
        ImageKind::Gif | ImageKind::WebP => {
            let mut frames = decode_animation(bytes)?.frames;
            let frame = frames.frame(0)?;
            (DecodedStill { rgba: frame.pixels, width: frame.width, height: frame.height }, true)
        },
        ImageKind::Avif => {
            let decoded = decode_avif(bytes)?;
            let animated = decoded.frame_count > 1;
            (avif_still_to_sdr(decoded), animated)
        },
        ImageKind::Platform => bail!("This image format needs a platform decoder."),
    };
    let still = match crop {
        Some(crop) => crop_still(still, crop)?,
        None => still,
    };
    let still = encode_sdr_still(still, profile)?;
    Ok(PreparedImage {
        bytes: still.bytes,
        width: still.width,
        height: still.height,
        animated: false,
        animation_fallback,
    })
}

/// Prepare a selected image for one of the product uses. Still and animated
/// inputs share their decoder, normalizer and AVIF writer; only the product
/// profile decides whether an animation is retained.
pub fn prepare_image(bytes: &[u8], request: MediaRequest) -> Result<PreparedImage> {
    request.validate().map_err(|error| anyhow::anyhow!(error))?;
    ensure!(
        u64::try_from(bytes.len())? <= MediaRequest::resource_limits().source_bytes,
        "The image exceeds the 100 MiB import limit."
    );
    let kind = inspect(bytes)?;
    let animation = request.animation_profile();
    let profile = request
        .image_profile()
        .or_else(|| animation.map(|profile| profile.image))
        .ok_or_else(|| anyhow::anyhow!("This request expects video input."))?;
    let animated = match kind {
        ImageKind::Gif | ImageKind::WebP => true,
        ImageKind::Png => ::image::codecs::png::PngDecoder::new(Cursor::new(bytes))?.is_apng()?,
        ImageKind::Avif => decode_avif(bytes)?.frame_count > 1,
        _ => false,
    };
    if !animated || animation.is_none() {
        let crop = match request {
            MediaRequest::Avatar { crop } => Some(crop),
            _ => None,
        };
        return prepare_sdr_image_with_crop(bytes, profile, crop);
    }
    prepare_animation(bytes, kind, animation.expect("checked above"))
}

pub fn validate_avatar_avif(bytes: &[u8]) -> Result<PreparedImage> {
    ensure!(
        inspect(bytes)? == ImageKind::Avif,
        "Profile pictures must be prepared as AVIF before saving"
    );
    prepare_image(
        bytes,
        MediaRequest::Avatar { crop: NormalizedCrop { center_x: 0.5, center_y: 0.5, zoom: 1.0 } },
    )
}

/// File-backed counterpart of [`prepare_image`]. It is used only after the
/// source lease has become a bounded, private core import.
pub fn prepare_image_path(path: &Path, request: MediaRequest) -> Result<PreparedImage> {
    request.validate().map_err(|error| anyhow::anyhow!(error))?;
    let kind = inspect_path(path)?;
    let animation = request.animation_profile();
    let profile = request
        .image_profile()
        .or_else(|| animation.map(|profile| profile.image))
        .ok_or_else(|| anyhow::anyhow!("This request expects video input."))?;
    let animated = match kind {
        ImageKind::Gif | ImageKind::WebP => true,
        ImageKind::Png => {
            ::image::codecs::png::PngDecoder::new(BufReader::new(File::open(path)?))?.is_apng()?
        },
        ImageKind::Avif => decode_avif_path(path)?.frame_count > 1,
        _ => false,
    };
    if animated && let Some(animation) = animation {
        let frames = match kind {
            ImageKind::Avif => decode_avif_animation_path(path)?,
            _ => decode_animation_path(path)?,
        };
        return prepare_animation_frames(frames, animation);
    }
    let crop = match request {
        MediaRequest::Avatar { crop } => Some(crop),
        _ => None,
    };
    prepare_sdr_image_path(path, kind, profile, crop)
}

/// Normalizes a platform-only image format without letting the platform choose
/// the product profile or the AVIF output.
pub fn prepare_platform_image(
    source: &mut SourceLease, request: MediaRequest,
) -> Result<PreparedImage> {
    request.validate().map_err(|error| anyhow::anyhow!(error))?;
    let profile = request
        .image_profile()
        .or_else(|| request.animation_profile().map(|profile| profile.image))
        .ok_or_else(|| anyhow::anyhow!("This request expects video input."))?;
    let probe = source.inspect_image()?;
    validate_dimensions(probe.width, probe.height)?;
    let crop = match request {
        MediaRequest::Avatar { crop } => Some(crop),
        _ => None,
    };
    let decoded = source.decode_image(ImageFrameRequest {
        max_edge: platform_decode_edge(profile, crop, probe.width, probe.height),
    })?;
    validate_dimensions(decoded.width, decoded.height)?;
    ensure!(
        decoded.format == PixelFormat::Rgba8,
        "This device cannot convert the image to SDR pixels."
    );
    if probe.source_is_hdr || decoded.source_is_hdr {
        super::alert(crate::platform::MediaAlert::HdrConvertedToSdr);
    }
    if probe.animated {
        super::alert(crate::platform::MediaAlert::AnimationSimplified);
    }
    let still = DecodedStill { rgba: decoded.pixels, width: decoded.width, height: decoded.height };
    let still = match crop {
        Some(crop) => crop_still(still, crop)?,
        None => still,
    };
    let still = encode_sdr_still(still, profile)?;
    Ok(PreparedImage {
        bytes: still.bytes,
        width: still.width,
        height: still.height,
        animated: false,
        animation_fallback: probe.animated,
    })
}

fn platform_decode_edge(
    profile: ImageProfile, crop: Option<NormalizedCrop>, source_width: u32, source_height: u32,
) -> u32 {
    let edge = crop
        .map(|crop| (profile.max_edge as f32 * crop.zoom).ceil() as u32)
        .unwrap_or(profile.max_edge);
    edge.clamp(1, source_width.max(source_height))
}

fn prepare_animation(
    bytes: &[u8], kind: ImageKind, profile: super::policy::AnimationProfile,
) -> Result<PreparedImage> {
    let frames = match kind {
        ImageKind::Avif => decode_avif_animation(bytes)?,
        _ => decode_animation(bytes)?,
    };
    prepare_animation_frames(frames, profile)
}

pub(crate) fn prepare_animation_frames(
    mut frames: DecodedFrames, profile: super::policy::AnimationProfile,
) -> Result<PreparedImage> {
    let fallback = frames.frames.frame(0)?;
    let fallback =
        DecodedStill { rgba: fallback.pixels, width: fallback.width, height: fallback.height };
    match encode_sdr_animation(&mut frames.frames, profile) {
        Ok((bytes, width, height)) => {
            Ok(PreparedImage { bytes, width, height, animated: true, animation_fallback: false })
        },
        Err(_) => {
            super::alert(crate::platform::MediaAlert::AnimationSimplified);
            let still = encode_sdr_still(fallback, profile.image)?;
            Ok(PreparedImage {
                bytes: still.bytes,
                width: still.width,
                height: still.height,
                animated: false,
                animation_fallback: true,
            })
        },
    }
}

fn prepare_sdr_image_path(
    path: &Path, kind: ImageKind, profile: ImageProfile, crop: Option<NormalizedCrop>,
) -> Result<PreparedImage> {
    let (still, animation_fallback) = match kind {
        ImageKind::Jpeg => (decode_sdr_still_path(path)?, false),
        ImageKind::Png => {
            let decoder = ::image::codecs::png::PngDecoder::new(BufReader::new(File::open(path)?))?;
            if !decoder.is_apng()? {
                (decode_sdr_still_path(path)?, false)
            } else {
                let mut frames = decode_animation_path(path)?.frames;
                let frame = frames.frame(0)?;
                (
                    DecodedStill { rgba: frame.pixels, width: frame.width, height: frame.height },
                    true,
                )
            }
        },
        ImageKind::Gif | ImageKind::WebP => {
            let mut frames = decode_animation_path(path)?.frames;
            let frame = frames.frame(0)?;
            (DecodedStill { rgba: frame.pixels, width: frame.width, height: frame.height }, true)
        },
        ImageKind::Avif => {
            let decoded = decode_avif_path(path)?;
            let animated = decoded.frame_count > 1;
            (avif_still_to_sdr(decoded), animated)
        },
        ImageKind::Platform => bail!("This image format needs a platform decoder."),
    };
    let still = match crop {
        Some(crop) => crop_still(still, crop)?,
        None => still,
    };
    let still = encode_sdr_still(still, profile)?;
    Ok(PreparedImage {
        bytes: still.bytes,
        width: still.width,
        height: still.height,
        animated: false,
        animation_fallback,
    })
}

fn avif_still_to_sdr(decoded: DecodedAvif) -> DecodedStill {
    let hdr = matches!(decoded.color.transfer, 16 | 18);
    if hdr {
        super::alert(crate::platform::MediaAlert::HdrConvertedToSdr);
    }
    let rgba = rgba16_to_sdr(&decoded.frame.pixels, decoded.color.transfer);
    DecodedStill { rgba, width: decoded.frame.width, height: decoded.frame.height }
}

fn rgba16_to_sdr(pixels: &[u8], transfer: u16) -> Vec<u8> {
    let hdr = matches!(transfer, 16 | 18);
    pixels
        .chunks_exact(8)
        .flat_map(|pixel| {
            let channel = |index| u16::from_ne_bytes([pixel[index], pixel[index + 1]]) as f32 / 65_535.0;
            let (r, g, b) = (channel(0), channel(2), channel(4));
            let (r, g, b) = if hdr { hdr_to_sdr(r, g, b, transfer) } else { (r, g, b) };
            [linear_to_srgb(r), linear_to_srgb(g), linear_to_srgb(b), (channel(6) * 255.0).round() as u8]
        })
        .collect()
}

fn hdr_to_sdr(r: f32, g: f32, b: f32, transfer: u16) -> (f32, f32, f32) {
    let decode = match transfer {
        16 => pq_to_nits,
        18 => hlg_to_nits,
        _ => unreachable!(),
    };
    // libavif gives Rec.2020 RGB for HDR AVIF. Convert to linear sRGB before
    // the SDR shoulder so saturated highlights retain their hue.
    let (r, g, b) = (decode(r), decode(g), decode(b));
    let (r, g, b) = (
        1.6605 * r - 0.5876 * g - 0.0728 * b,
        -0.1246 * r + 1.1329 * g - 0.0083 * b,
        -0.0182 * r - 0.1006 * g + 1.1187 * b,
    );
    let shoulder = |nits: f32| {
        let relative = (nits / 203.0).max(0.0);
        relative / (relative + 0.25)
    };
    (shoulder(r), shoulder(g), shoulder(b))
}

fn pq_to_nits(encoded: f32) -> f32 {
    let encoded = encoded.clamp(0.0, 1.0);
    let p = encoded.powf(1.0 / 78.84375);
    let linear = ((p - 0.8359375).max(0.0) / (18.8515625 - 18.6875 * p)).powf(1.0 / 0.15930176);
    linear * 10_000.0
}

fn hlg_to_nits(encoded: f32) -> f32 {
    let encoded = encoded.clamp(0.0, 1.0);
    let linear = if encoded <= 0.5 {
        encoded * encoded / 3.0
    } else {
        (((encoded - 0.55991073) / 0.17883277).exp() + 0.28466892) / 12.0
    };
    linear * 1_000.0
}

fn decode_avif_animation(bytes: &[u8]) -> Result<DecodedFrames> {
    let (first, source) = decode_avif_frames(bytes)?;
    normalize_avif_frames(first, source)
}

fn decode_avif_animation_path(path: &Path) -> Result<DecodedFrames> {
    let (first, source) = decode_avif_frames_path(path)?;
    normalize_avif_frames(first, source)
}

fn normalize_avif_frames(first: DecodedAvif, mut source: FrameStore) -> Result<DecodedFrames> {
    if matches!(first.color.transfer, 16 | 18) {
        super::alert(crate::platform::MediaAlert::HdrConvertedToSdr);
    }
    let limits = MediaRequest::resource_limits();
    let mut frames = FrameStore::new(limits.frame_store_memory, limits.job_bytes);
    for index in 0..source.len() {
        let frame = source.frame(index)?;
        let pixels = rgba16_to_sdr(&frame.pixels, first.color.transfer);
        frames.push(Frame::new(
            pixels,
            frame.width,
            frame.height,
            PixelFormat::Rgba8,
            frame.duration_ms.max(10),
        )?)?;
    }
    Ok(DecodedFrames { width: first.frame.width, height: first.frame.height, frames })
}

pub fn crop_still(decoded: DecodedStill, crop: NormalizedCrop) -> Result<DecodedStill> {
    crop.validate().map_err(|error| anyhow::anyhow!(error))?;
    let image = ::image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba)
        .ok_or_else(|| anyhow::anyhow!("invalid decoded image"))?;
    let side = ((decoded.width.min(decoded.height) as f32 / crop.zoom).round() as u32).max(1);
    let center_x = crop.center_x * decoded.width as f32;
    let center_y = crop.center_y * decoded.height as f32;
    let x = (center_x - side as f32 / 2.0).round().clamp(0.0, (decoded.width - side) as f32) as u32;
    let y =
        (center_y - side as f32 / 2.0).round().clamp(0.0, (decoded.height - side) as f32) as u32;
    let image = ::image::imageops::crop_imm(&image, x, y, side, side).to_image();
    Ok(DecodedStill { rgba: image.into_raw(), width: side, height: side })
}

/// Fully composited frames from core-owned animated formats. The caller can
/// sample, resize, or collapse this same store without re-decoding the source.
pub fn decode_animation(bytes: &[u8]) -> Result<DecodedFrames> {
    match inspect(bytes)? {
        ImageKind::Gif => {
            collect_frames(::image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?.into_frames())
        },
        ImageKind::Png => {
            let decoder = ::image::codecs::png::PngDecoder::new(Cursor::new(bytes))?;
            ensure!(decoder.is_apng()?, "The PNG is not animated.");
            collect_frames(decoder.apng()?.into_frames())
        },
        ImageKind::WebP => collect_frames(
            ::image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))?.into_frames(),
        ),
        _ => bail!("This image format does not use the core animation decoder."),
    }
}

fn collect_frames(
    decoded: impl Iterator<Item = ::image::ImageResult<::image::Frame>>,
) -> Result<DecodedFrames> {
    let limits = MediaRequest::resource_limits();
    let mut store = FrameStore::new(limits.frame_store_memory, limits.job_bytes);
    let mut dimensions = None;
    let mut duration = 0_u64;
    for decoded in decoded {
        let decoded = decoded?;
        let (numerator, denominator) = decoded.delay().numer_denom_ms();
        let image = decoded.into_buffer();
        validate_dimensions(image.width(), image.height())?;
        let (width, height) = (image.width(), image.height());
        if let Some(dimensions) = dimensions {
            ensure!(
                dimensions == (width, height),
                "Animated image frames have different canvas sizes."
            );
        } else {
            dimensions = Some((width, height));
        }
        let frame_duration = u64::from(numerator).div_ceil(u64::from(denominator.max(1))).max(10);
        duration = duration.saturating_add(frame_duration);
        ensure!(
            duration <= limits.animation_duration_ms,
            "The animation exceeds the five minute import limit."
        );
        ensure!(
            store.len() < limits.animation_frames as usize,
            "The animation exceeds the 600 frame import limit."
        );
        store.push(Frame::new(
            image.into_raw(),
            width,
            height,
            PixelFormat::Rgba8,
            u32::try_from(frame_duration)?,
        )?)?;
    }
    let (width, height) =
        dimensions.ok_or_else(|| anyhow::anyhow!("The animation has no frames."))?;
    Ok(DecodedFrames { width, height, frames: store })
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

/// Writes a new, metadata-free SDR AVIF through a finite quality and spatial
/// ladder. The input pixels are already oriented by the decoder.
pub fn encode_sdr_still(decoded: DecodedStill, profile: ImageProfile) -> Result<PreparedStill> {
    let image = ::image::RgbaImage::from_raw(decoded.width, decoded.height, decoded.rgba)
        .ok_or_else(|| anyhow::anyhow!("invalid decoded image"))?;
    let mut edge = decoded.width.max(decoded.height).min(profile.max_edge);
    loop {
        let (width, height) = fit_dimensions(decoded.width, decoded.height, edge);
        let scaled = resize_linear_srgb(&image, width, height);
        for quality in [72.0, 58.0, 44.0] {
            let bytes = encode_sdr_avif(scaled.as_raw(), width, height, quality)?;
            if bytes.len() as u64 <= profile.max_bytes {
                return Ok(PreparedStill { bytes, width, height });
            }
        }
        ensure!(edge > profile.emergency_edge, "This image cannot fit within the send limit.");
        edge = (edge * 3 / 4).max(profile.emergency_edge);
    }
}

pub(super) fn encode_sdr_avif(
    rgba: &[u8], width: u32, height: u32, quality: f32,
) -> Result<Vec<u8>> {
    let mut encoder = new_avif_encoder(width, height, quality, false)?;
    encoder
        .add_rgba(rgba, 1)
        .map_err(|error| anyhow::anyhow!("AVIF encoder rejected the image: {error:?}"))?;
    encoder.finish().map_err(|error| anyhow::anyhow!("AVIF encoder rejected the image: {error:?}"))
}

fn new_avif_encoder(
    width: u32, height: u32, quality: f32, animated: bool,
) -> Result<promtuz_avif::Encoder> {
    let quantizer = ((100.0 - quality.clamp(0.0, 100.0)) * 63.0 / 100.0).round() as u8;
    promtuz_avif::Encoder::new(
        width,
        height,
        promtuz_avif::EncodeConfig { quantizer, animated, ..promtuz_avif::EncodeConfig::default() },
    )
    .map_err(|error| anyhow::anyhow!("AVIF encoder rejected the image: {error:?}"))
}

fn encode_sdr_animation(
    frames: &mut FrameStore, profile: super::policy::AnimationProfile,
) -> Result<(Vec<u8>, u32, u32)> {
    ensure!(frames.len() > 1, "The animation has no additional frames.");
    let first = frames.frame(0)?;
    ensure!(
        first.format == PixelFormat::Rgba8,
        "The animation must be normalized to SDR pixels before encoding."
    );
    let samples = select_animation_frames(frames, &first, profile)?;
    let mut edge = first.width.max(first.height).min(profile.image.max_edge);
    let mut last_size = 0_u64;
    for (attempt, quality) in [72.0, 58.0, 44.0].into_iter().enumerate() {
        if attempt >= 2 && last_size > profile.image.max_bytes {
            let scale = (0.82 * profile.image.max_bytes as f64 / last_size as f64).sqrt();
            edge = ((f64::from(edge) * scale) as u32).max(profile.image.emergency_edge).min(edge);
        }
        let (width, height) = fit_dimensions(first.width, first.height, edge);
        let bytes = encode_animation_candidate(frames, &samples, width, height, quality)?;
        last_size = u64::try_from(bytes.len())?;
        if last_size <= profile.image.max_bytes {
            return Ok((bytes, width, height));
        }
    }
    bail!("This animation cannot fit within the send limit without removing frames")
}

fn select_animation_frames(
    frames: &mut FrameStore, first: &Frame, profile: super::policy::AnimationProfile,
) -> Result<Vec<(usize, u32)>> {
    let interval = 1_000_u32.div_ceil(profile.max_fps.max(1));
    let mut total_duration = 0_u64;
    let mut samples = Vec::new();
    let mut pending = Some((0, first.duration_ms.max(10)));
    for index in 0..frames.len() {
        let frame = if index == 0 { first.clone() } else { frames.frame(index)? };
        ensure!(
            frame.format == PixelFormat::Rgba8
                && (frame.width, frame.height) == (first.width, first.height),
            "Animated image frames have different pixel layouts."
        );
        let duration = frame.duration_ms.max(10);
        total_duration = total_duration.saturating_add(u64::from(duration));
        if let Some(max_duration) = profile.max_duration_ms {
            ensure!(total_duration <= max_duration, "This animation is too long to send.");
        }
        if index == 0 {
            continue;
        }
        let (pending_index, pending_duration) = pending.expect("first frame is pending");
        if pending_duration.saturating_add(duration) < interval {
            pending = Some((pending_index, pending_duration.saturating_add(duration)));
            continue;
        }
        ensure!(
            u32::try_from(samples.len()).unwrap_or(u32::MAX) < profile.max_frames,
            "This animation has too many frames to send."
        );
        samples.push((pending_index, pending_duration));
        pending = Some((index, duration));
    }
    let pending = pending.expect("first frame is pending");
    ensure!(
        u32::try_from(samples.len()).unwrap_or(u32::MAX) < profile.max_frames,
        "This animation has too many frames to send."
    );
    samples.push(pending);
    ensure!(samples.len() > 1, "The animation has no additional frames.");
    Ok(samples)
}

fn encode_animation_candidate(
    frames: &mut FrameStore, samples: &[(usize, u32)], width: u32, height: u32, quality: f32,
) -> Result<Vec<u8>> {
    ensure!(width >= 16 && height >= 16, "This animation is too small to encode.");
    let mut encoder = new_avif_encoder(width, height, quality, true)?;
    for &(index, duration) in samples {
        let frame = frames.frame(index)?;
        let image = ::image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
            .ok_or_else(|| anyhow::anyhow!("invalid normalized animation frame"))?;
        let scaled = resize_linear_srgb(&image, width, height);
        encoder
            .add_rgba(scaled.as_raw(), duration)
            .map_err(|error| anyhow::anyhow!("AVIF encoder rejected the animation: {error:?}"))?;
    }
    encoder
        .finish()
        .map_err(|error| anyhow::anyhow!("AVIF encoder rejected the animation: {error:?}"))
}

pub(super) fn resize_linear_srgb(
    image: &::image::RgbaImage, target_width: u32, target_height: u32,
) -> ::image::RgbaImage {
    if (image.width(), image.height()) == (target_width, target_height) {
        return image.clone();
    }
    let mut output = vec![0; target_width as usize * target_height as usize * 4];
    let decode = srgb_decode_table();
    let x_scale = image.width() as f32 / target_width as f32;
    let y_scale = image.height() as f32 / target_height as f32;
    for target_y in 0..target_height {
        let (y0, y1, y_weight) = sample_axis(target_y, y_scale, image.height());
        for target_x in 0..target_width {
            let (x0, x1, x_weight) = sample_axis(target_x, x_scale, image.width());
            let pixels = [
                image.get_pixel(x0, y0).0,
                image.get_pixel(x1, y0).0,
                image.get_pixel(x0, y1).0,
                image.get_pixel(x1, y1).0,
            ];
            let weights = [
                (1.0 - x_weight) * (1.0 - y_weight),
                x_weight * (1.0 - y_weight),
                (1.0 - x_weight) * y_weight,
                x_weight * y_weight,
            ];
            let mut rgba = [0.0; 4];
            for (pixel, weight) in pixels.iter().zip(weights) {
                let alpha = f32::from(pixel[3]) / 255.0;
                rgba[0] += decode[pixel[0] as usize] * alpha * weight;
                rgba[1] += decode[pixel[1] as usize] * alpha * weight;
                rgba[2] += decode[pixel[2] as usize] * alpha * weight;
                rgba[3] += alpha * weight;
            }
            let offset = ((target_y * target_width + target_x) * 4) as usize;
            output[offset + 3] = (rgba[3] * 255.0).round().clamp(0.0, 255.0) as u8;
            if rgba[3] > f32::EPSILON {
                output[offset] = linear_to_srgb(rgba[0] / rgba[3]);
                output[offset + 1] = linear_to_srgb(rgba[1] / rgba[3]);
                output[offset + 2] = linear_to_srgb(rgba[2] / rgba[3]);
            }
        }
    }
    ::image::RgbaImage::from_raw(target_width, target_height, output).expect("valid output shape")
}

fn sample_axis(target: u32, scale: f32, source_length: u32) -> (u32, u32, f32) {
    let source = ((target as f32 + 0.5) * scale - 0.5).clamp(0.0, source_length as f32 - 1.0);
    let first = source.floor() as u32;
    (first, (first + 1).min(source_length - 1), source - first as f32)
}

fn srgb_decode_table() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|value| {
            let value = value as f32 / 255.0;
            if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
        })
    })
}

fn linear_to_srgb(value: f32) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let encoded =
        if value <= 0.003_130_8 { value * 12.92 } else { 1.055 * value.powf(1.0 / 2.4) - 0.055 };
    (encoded * 255.0).round() as u8
}

fn fit_dimensions(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let scale = (f64::from(edge) / f64::from(width.max(height))).min(1.0);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

pub fn decode_avif(bytes: &[u8]) -> Result<DecodedAvif> {
    let decoder = promtuz_avif::Decoder::open(bytes, avif_limits()?)
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
    decode_avif_decoder(decoder)
}

pub fn decode_avif_path(path: &Path) -> Result<DecodedAvif> {
    let decoder = promtuz_avif::Decoder::open_file(path, avif_limits()?)
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
    decode_avif_decoder(decoder)
}

fn avif_limits() -> Result<promtuz_avif::DecodeLimits> {
    let limits = MediaRequest::resource_limits();
    Ok(promtuz_avif::DecodeLimits {
        max_pixels: u32::try_from(limits.source_pixels)?,
        max_edge: 8_192,
        max_frames: limits.animation_frames,
    })
}

fn decode_avif_decoder(mut decoder: promtuz_avif::Decoder) -> Result<DecodedAvif> {
    let image = decoder
        .decode_next()
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?
        .ok_or_else(|| anyhow::anyhow!("AVIF contains no frames"))?;
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

pub fn decode_avif_frames(bytes: &[u8]) -> Result<(DecodedAvif, FrameStore)> {
    let decoder = promtuz_avif::Decoder::open(bytes, avif_limits()?)
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
    decode_avif_frames_decoder(decoder)
}

pub fn decode_avif_frames_path(path: &Path) -> Result<(DecodedAvif, FrameStore)> {
    let decoder = promtuz_avif::Decoder::open_file(path, avif_limits()?)
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
    decode_avif_frames_decoder(decoder)
}

fn decode_avif_frames_decoder(
    mut decoder: promtuz_avif::Decoder,
) -> Result<(DecodedAvif, FrameStore)> {
    let limits = MediaRequest::resource_limits();
    let mut frames = FrameStore::new(limits.frame_store_memory, limits.job_bytes);
    let mut first = None;
    while let Some(image) = decoder
        .decode_next()
        .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?
    {
        let pixels = image.rgba16().iter().flat_map(|pixel| pixel.to_ne_bytes()).collect();
        let frame =
            Frame::new(pixels, image.width, image.height, PixelFormat::Rgba16, image.duration_ms)?;
        if first.is_none() {
            first = Some(DecodedAvif {
                frame: frame.clone(),
                color: image.color,
                source_depth: image.source_depth,
                frame_count: image.frame_count,
                has_alpha: image.has_alpha,
                has_gain_map: image.has_gain_map,
            });
        }
        frames.push(frame)?;
    }
    Ok((first.ok_or_else(|| anyhow::anyhow!("AVIF contains no frames"))?, frames))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

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
    fn avif_frames_share_the_bounded_frame_store() {
        let encoded = super::super::encode_avif(&[255, 0, 0, 255], 1, 1, 60.0).unwrap();
        let (first, mut frames) = decode_avif_frames(&encoded).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames.frame(0).unwrap(), first.frame);
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

    #[test]
    fn bmff_images_only_use_libavif_when_they_declare_an_avif_brand() {
        let mut avif = b"\0\0\0\x18ftypavif\0\0\0\0mif1avis".to_vec();
        avif.resize(64, 0);
        assert_eq!(inspect(&avif).unwrap(), ImageKind::Avif);

        let mut heif = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        heif.resize(64, 0);
        assert_eq!(inspect(&heif).unwrap(), ImageKind::Platform);
    }

    #[test]
    fn jxl_is_a_platform_media_candidate_not_a_raw_attachment() {
        assert_eq!(inspect(&[0xff, 0x0a]).unwrap(), ImageKind::Platform);
        assert_eq!(
            inspect(&[0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a]).unwrap(),
            ImageKind::Platform
        );
        assert!(!platform_image_candidate(b"\0\0\0\x18ftypM4A \0\0\0\0isom"));
    }

    #[test]
    fn hdr_transfer_functions_produce_finite_sdr_pixels() {
        assert!(pq_to_nits(0.0).abs() < f32::EPSILON);
        assert!((9_000.0..=10_000.0).contains(&pq_to_nits(1.0)));
        assert!((999.0..=1_001.0).contains(&hlg_to_nits(1.0)));
        for transfer in [16, 18] {
            let pixel = hdr_to_sdr(1.0, 0.5, 0.25, transfer);
            assert!(pixel.0.is_finite() && pixel.1.is_finite() && pixel.2.is_finite());
            assert!((0.0..=1.0).contains(&pixel.0));
        }
    }

    #[test]
    fn private_import_path_uses_the_same_core_decoder() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("picked.png");
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[1, 2, 3, 4], 1, 1, ColorType::Rgba8.into())
            .unwrap();
        File::create(&path).unwrap().write_all(&png).unwrap();
        assert_eq!(inspect_path(&path).unwrap(), ImageKind::Png);
        assert_eq!(decode_sdr_still_path(&path).unwrap().rgba, vec![1, 2, 3, 4]);
        let prepared =
            prepare_image_path(&path, MediaRequest::ChatImage { hd_enabled: false }).unwrap();
        assert_eq!(inspect(&prepared.bytes).unwrap(), ImageKind::Avif);
    }

    #[test]
    fn private_avif_import_is_reencoded_from_libavif_file_io() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("picked.avif");
        File::create(&path)
            .unwrap()
            .write_all(&super::super::encode_avif(&[1, 2, 3, 4], 1, 1, 60.0).unwrap())
            .unwrap();
        let prepared =
            prepare_image_path(&path, MediaRequest::ChatImage { hd_enabled: false }).unwrap();
        assert_eq!(inspect(&prepared.bytes).unwrap(), ImageKind::Avif);
    }

    #[test]
    fn animated_gif_becomes_timed_frames() {
        const GIF: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0\x3b";
        let mut decoded = decode_animation(GIF).unwrap();
        assert_eq!((decoded.width, decoded.height, decoded.frames.len()), (1, 1, 1));
        assert_eq!(decoded.frames.frame(0).unwrap().duration_ms, 10);
    }

    #[test]
    fn still_encoding_uses_the_emergency_rung_before_refusing() {
        let source = DecodedStill { rgba: vec![0; 4 * 8 * 8], width: 8, height: 8 };
        let rendered = encode_sdr_still(
            source,
            ImageProfile { max_edge: 8, max_bytes: 4_096, emergency_edge: 2 },
        )
        .unwrap();
        assert!(rendered.bytes.len() <= 4_096);
        assert_eq!((rendered.width, rendered.height), (8, 8));
    }

    #[test]
    fn animated_input_has_one_composited_fallback_image() {
        const GIF: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0\x3b";
        let prepared = prepare_sdr_image(
            GIF,
            ImageProfile { max_edge: 512, max_bytes: 256 * 1024, emergency_edge: 128 },
        )
        .unwrap();
        assert!(prepared.animation_fallback);
        assert_eq!(inspect(&prepared.bytes).unwrap(), ImageKind::Avif);
    }

    #[test]
    fn normalized_animation_uses_the_libavif_sequence_writer() {
        let mut frames = FrameStore::new(4_096, 4_096);
        for value in [0, 255] {
            frames
                .push(
                    Frame::new(
                        vec![value, 0, 0, 255].repeat(16 * 16),
                        16,
                        16,
                        PixelFormat::Rgba8,
                        100,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let prepared = prepare_animation_frames(
            DecodedFrames { width: 16, height: 16, frames },
            super::super::policy::CHAT_ANIMATION,
        )
        .unwrap();
        let info = super::super::inspect_avif(&prepared.bytes).unwrap().unwrap();
        assert!(prepared.animated);
        assert!(!prepared.animation_fallback);
        assert_eq!(info.frame_count, 2);
        assert_eq!(info.duration_ms, Some(200));
    }

    #[test]
    fn tiny_animations_fall_back_to_the_first_composited_frame() {
        let mut frames = FrameStore::new(1_024, 1_024);
        for value in [0, 255] {
            frames
                .push(Frame::new(vec![value, 0, 0, 255], 1, 1, PixelFormat::Rgba8, 100).unwrap())
                .unwrap();
        }
        let prepared = prepare_animation_frames(
            DecodedFrames { width: 1, height: 1, frames },
            super::super::policy::CHAT_ANIMATION,
        )
        .unwrap();
        assert!(!prepared.animated);
        assert!(prepared.animation_fallback);
    }

    #[test]
    fn crop_is_square_and_clamped_to_the_source() {
        let cropped = crop_still(
            DecodedStill { rgba: vec![0; 4 * 4 * 2], width: 4, height: 2 },
            NormalizedCrop { center_x: 1.0, center_y: 0.0, zoom: 1.0 },
        )
        .unwrap();
        assert_eq!((cropped.width, cropped.height, cropped.rgba.len()), (2, 2, 16));
    }

    #[test]
    fn resize_interpolates_premultiplied_linear_pixels() {
        let source =
            ::image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 0, 0, 0, 255, 255]).unwrap();
        let scaled = resize_linear_srgb(&source, 1, 1);
        assert_eq!(scaled.as_raw()[3], 128);
        assert!(scaled.as_raw()[2] >= 254);
        assert_eq!(&scaled.as_raw()[..2], &[0, 0]);
    }
}
