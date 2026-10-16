use std::{
    fs::File,
    io::{BufReader, Cursor, Read},
    path::Path,
};

use ::image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use anyhow::{Result, bail, ensure};
use ravif::{BitDepth, Encoder, Img};
use rgb::FromSlice;

use super::{
    frames::{Frame, FrameStore, PixelFormat},
    policy::{ImageProfile, MediaRequest, NormalizedCrop},
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

/// The private import is the only file path media decoding accepts. Picker URIs
/// never reach this layer, and ordinary formats stream from that private copy.
pub fn inspect_path(path: &Path) -> Result<ImageKind> {
    let mut header = [0_u8; 64];
    let used = File::open(path)?.read(&mut header)?;
    let header = &header[..used];
    if header.len() >= 12 && &header[4..8] == b"ftyp" {
        return Ok(ImageKind::Avif);
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
            ensure!(
                !matches!(decoded.color.transfer, 16 | 18),
                "HDR AVIF needs tone mapping before SDR conversion."
            );
            let rgba = decoded
                .frame
                .pixels
                .chunks_exact(2)
                .map(|channel| (u16::from_ne_bytes([channel[0], channel[1]]) >> 8) as u8)
                .collect();
            (
                DecodedStill { rgba, width: decoded.frame.width, height: decoded.frame.height },
                decoded.frame_count > 1,
            )
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
        ImageKind::Avif => bail!("AVIF file decoding has not been configured."),
        _ => false,
    };
    if animated && let Some(animation) = animation {
        return prepare_animation_frames(decode_animation_path(path)?, animation);
    }
    let crop = match request {
        MediaRequest::Avatar { crop } => Some(crop),
        _ => None,
    };
    prepare_sdr_image_path(path, kind, profile, crop)
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

fn prepare_animation_frames(
    mut frames: DecodedFrames, profile: super::policy::AnimationProfile,
) -> Result<PreparedImage> {
    let fallback = frames.frames.frame(0)?;
    let fallback =
        DecodedStill { rgba: fallback.pixels, width: fallback.width, height: fallback.height };
    match super::gif::encode_frames(&mut frames.frames, profile) {
        Ok(prepared) => Ok(PreparedImage {
            bytes: prepared.bytes,
            width: prepared.width,
            height: prepared.height,
            animated: prepared.animated,
            animation_fallback: false,
        }),
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
        ImageKind::Avif => bail!("AVIF file decoding has not been configured."),
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

fn decode_avif_animation(bytes: &[u8]) -> Result<DecodedFrames> {
    let (first, mut source) = decode_avif_frames(bytes)?;
    ensure!(
        !matches!(first.color.transfer, 16 | 18),
        "HDR AVIF needs tone mapping before SDR conversion."
    );
    let limits = MediaRequest::resource_limits();
    let mut frames = FrameStore::new(limits.frame_store_memory, limits.job_bytes);
    for index in 0..source.len() {
        let frame = source.frame(index)?;
        let pixels = frame
            .pixels
            .chunks_exact(2)
            .map(|channel| (u16::from_ne_bytes([channel[0], channel[1]]) >> 8) as u8)
            .collect();
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
        let scaled = if (width, height) == (decoded.width, decoded.height) {
            image.clone()
        } else {
            ::image::imageops::resize(
                &image,
                width,
                height,
                ::image::imageops::FilterType::Lanczos3,
            )
        };
        for quality in [72.0, 58.0, 44.0] {
            let bytes = Encoder::new()
                .with_quality(quality)
                .with_alpha_quality(85.0)
                .with_speed(8)
                .with_bit_depth(BitDepth::Eight)
                .encode_rgba(Img::new(scaled.as_raw().as_rgba(), width as usize, height as usize))?
                .avif_file;
            if bytes.len() as u64 <= profile.max_bytes {
                return Ok(PreparedStill { bytes, width, height });
            }
        }
        ensure!(edge > profile.emergency_edge, "This image cannot fit within the send limit.");
        edge = (edge * 3 / 4).max(profile.emergency_edge);
    }
}

fn fit_dimensions(width: u32, height: u32, edge: u32) -> (u32, u32) {
    let scale = (f64::from(edge) / f64::from(width.max(height))).min(1.0);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
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

pub fn decode_avif_frames(bytes: &[u8]) -> Result<(DecodedAvif, FrameStore)> {
    let limits = MediaRequest::resource_limits();
    let mut decoder = promtuz_avif::Decoder::open(
        bytes,
        promtuz_avif::DecodeLimits {
            max_pixels: u32::try_from(limits.source_pixels)?,
            max_edge: 8_192,
            max_frames: limits.animation_frames,
        },
    )
    .map_err(|error| anyhow::anyhow!("AVIF decoder rejected the image: {error:?}"))?;
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
    fn crop_is_square_and_clamped_to_the_source() {
        let cropped = crop_still(
            DecodedStill { rgba: vec![0; 4 * 4 * 2], width: 4, height: 2 },
            NormalizedCrop { center_x: 1.0, center_y: 0.0, zoom: 1.0 },
        )
        .unwrap();
        assert_eq!((cropped.width, cropped.height, cropped.rgba.len()), (2, 2, 16));
    }
}
