//! GIF disposal/compositing is handled by image's maintained GIF decoder.
//! Decode, resize and feed one frame at a time; never retain an RGBA movie.
use anyhow::{Result, anyhow, bail, ensure};
use image::{AnimationDecoder, ImageDecoder};
use rav1e::prelude::*;
use std::io::Cursor;

use super::{
    AvatarCrop, MAX_ENCODED_SOURCE_BYTES, MediaPolicy, PreparedImage,
    avatar::CropArea,
    avif::{MAX_DURATION_CS, MAX_FRAMES},
    fit_dimensions,
    sequence::{self, Sample, Track},
    validate_dimensions,
};

const MAX_GIF_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_GIF_PIXEL_WORK: u64 = 256 * 1024 * 1024;

struct GifInfo {
    width: u32,
    height: u32,
    delays: Vec<u32>,
    repetitions: Option<u16>,
    alpha: bool,
}

/// Read sub-blocks without decompressing. Besides resource bounds, this keeps
/// the distinction between no loop extension (play once) and infinite looping.
fn inspect(bytes: &[u8]) -> Result<GifInfo> {
    let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
        let slice = bytes
            .get(*pos..pos.checked_add(n).ok_or_else(|| anyhow!("GIF offset overflow"))?)
            .ok_or_else(|| anyhow!("Truncated GIF"))?;
        *pos += n;
        Ok(slice)
    };
    let mut pos = 6;
    let screen = take(&mut pos, 7)?;
    let width = u32::from(u16::from_le_bytes([screen[0], screen[1]]));
    let height = u32::from(u16::from_le_bytes([screen[2], screen[3]]));
    validate_dimensions(width, height)?;
    let pixels = u64::from(width) * u64::from(height);
    ensure!(pixels <= MAX_GIF_PIXELS, "GIF canvas exceeds 16 megapixels");
    if screen[4] & 0x80 != 0 {
        take(&mut pos, 3 << ((screen[4] & 7) + 1))?;
    }
    let mut info =
        GifInfo { width, height, delays: Vec::new(), repetitions: Some(0), alpha: false };
    let mut delay = 10;
    let mut duration = 0u64;
    loop {
        let tag = take(&mut pos, 1)?[0];
        match tag {
            0x3b => break,
            0x21 => {
                let label = take(&mut pos, 1)?[0];
                let mut blocks = 0;
                let mut looping = false;
                loop {
                    let n = usize::from(take(&mut pos, 1)?[0]);
                    if n == 0 {
                        break;
                    }
                    let data = take(&mut pos, n)?;
                    if label == 0xf9 {
                        ensure!(blocks == 0 && n == 4, "Invalid GIF graphics control extension");
                        let cs = u32::from(u16::from_le_bytes([data[1], data[2]]));
                        // Match browser GIF playback for unspecified/1cs delays.
                        delay = if cs <= 1 { 10 } else { cs };
                        info.alpha |= data[0] & 1 != 0;
                    }
                    if label == 0xff && blocks == 0 {
                        looping = data == b"NETSCAPE2.0" || data == b"ANIMEXTS1.0";
                    } else if looping && blocks == 1 {
                        ensure!(n == 3 && data[0] == 1, "Invalid GIF loop extension");
                        let repeats = u16::from_le_bytes([data[1], data[2]]);
                        info.repetitions = (repeats != 0).then_some(repeats);
                    }
                    blocks += 1;
                }
            },
            0x2c => {
                let frame = take(&mut pos, 9)?;
                let left = u32::from(u16::from_le_bytes([frame[0], frame[1]]));
                let top = u32::from(u16::from_le_bytes([frame[2], frame[3]]));
                let fw = u32::from(u16::from_le_bytes([frame[4], frame[5]]));
                let fh = u32::from(u16::from_le_bytes([frame[6], frame[7]]));
                ensure!(
                    fw > 0 && fh > 0 && left + fw <= width && top + fh <= height,
                    "GIF frame is outside its canvas"
                );
                if info.delays.is_empty() && (fw != width || fh != height) {
                    info.alpha = true;
                }
                if frame[8] & 0x80 != 0 {
                    take(&mut pos, 3 << ((frame[8] & 7) + 1))?;
                }
                let code_size = take(&mut pos, 1)?[0];
                ensure!((2..=8).contains(&code_size), "Invalid GIF LZW code size");
                loop {
                    let n = usize::from(take(&mut pos, 1)?[0]);
                    if n == 0 {
                        break;
                    }
                    take(&mut pos, n)?;
                }
                info.delays.push(delay);
                duration += u64::from(delay);
                ensure!(
                    info.delays.len() <= MAX_FRAMES as usize,
                    "GIF animations can contain at most {MAX_FRAMES} frames"
                );
                ensure!(duration <= MAX_DURATION_CS, "GIF animation exceeds five minutes");
                ensure!(
                    pixels * info.delays.len() as u64 <= MAX_GIF_PIXEL_WORK,
                    "GIF animation exceeds the processing limit"
                );
                delay = 10;
            },
            _ => bail!("Invalid GIF block"),
        }
    }
    ensure!(!info.delays.is_empty(), "GIF has no frames");
    Ok(info)
}

pub(super) fn prepare(bytes: &[u8], policy: MediaPolicy) -> Result<PreparedImage> {
    prepare_frames(bytes, policy, None)
}

pub(super) fn prepare_cropped(
    bytes: &[u8], policy: MediaPolicy, crop: AvatarCrop,
) -> Result<PreparedImage> {
    prepare_frames(bytes, policy, Some(crop))
}

fn prepare_frames(
    bytes: &[u8], policy: MediaPolicy, crop: Option<AvatarCrop>,
) -> Result<PreparedImage> {
    let info = inspect(bytes)?;
    let area = crop.map(|crop| crop.area(info.width, info.height)).transpose()?;
    let (source_width, source_height) =
        area.map_or((info.width, info.height), |area| (area.side, area.side));
    let mut edge = source_width.max(source_height).min(policy.max_edge).min(1600);
    let mut last_size = 0;
    // First lower quantization, then reduce dimensions using the actual output
    // size. Every retry decodes from the original GIF, without dropping frames.
    for (attempt, quantizer) in [85usize, 115, 135, 145].into_iter().enumerate() {
        if attempt >= 2 && last_size > policy.max_bytes {
            let scale = (0.82 * policy.max_bytes as f64 / last_size as f64).sqrt();
            edge = ((f64::from(edge) * scale) as u32).max(64).min(edge);
        }
        let (width, height) = fit_dimensions(source_width, source_height, edge);
        let result = encode(bytes, &info, width, height, quantizer, area)?;
        last_size = result.len();
        if result.len() <= policy.max_bytes {
            return Ok(PreparedImage {
                bytes: result,
                mime: "image/avif",
                width,
                height,
                animated: info.delays.len() > 1,
            });
        }
    }
    bail!(
        "This animation cannot fit within the {} byte limit without removing frames",
        policy.max_bytes
    )
}

struct Encoder {
    context: Context<u8>,
    // rav1e's inter-frame mode needs at least 16px on both axes. Independent
    // sync frames keep smaller sources/crops at their real dimensions while
    // the container still carries every frame and its presentation timing.
    independent: Option<Config>,
    track: Track,
    encoded_bytes: usize,
}
impl Encoder {
    fn new(width: u32, height: u32, quantizer: usize, alpha: bool) -> Result<Self> {
        let mut config = EncoderConfig::with_speed_preset(8);
        config.width = width as usize;
        config.height = height as usize;
        config.still_picture = width < 16 || height < 16;
        config.bit_depth = 8;
        config.chroma_sampling = if alpha { ChromaSampling::Cs400 } else { ChromaSampling::Cs444 };
        config.pixel_range = PixelRange::Full;
        config.time_base = Rational::new(1, 100);
        config.low_latency = true;
        config.speed_settings.rdo_lookahead_frames = 1;
        config.min_key_frame_interval = 0;
        config.max_key_frame_interval = 120;
        config.quantizer = if alpha { quantizer.min(65) } else { quantizer };
        config.color_description = Some(ColorDescription {
            color_primaries: ColorPrimaries::BT709,
            transfer_characteristics: TransferCharacteristics::SRGB,
            matrix_coefficients: MatrixCoefficients::BT601,
        });
        let independent = config.still_picture;
        let config = Config::new().with_encoder_config(config).with_threads(2);
        let context: Context<u8> = config.new_context()?;
        let mut header = context.container_sequence_header();
        // AV1 monochrome has subsampling_x = subsampling_y = 1. rav1e's
        // container helper in 0.7 omits y for Cs400; correct the container flag.
        if alpha {
            header[2] |= 4;
        }
        Ok(Self {
            context,
            independent: independent.then_some(config),
            track: Track { config: header, samples: Vec::new() },
            encoded_bytes: 0,
        })
    }

    fn feed(&mut self, rgba: &image::RgbaImage, alpha: bool) -> Result<()> {
        if let Some(config) = &self.independent
            && !self.track.samples.is_empty()
        {
            self.context = config.new_context()?;
        }
        let mut frame = self.context.new_frame();
        let width = rgba.width() as usize;
        let height = rgba.height() as usize;
        for (plane_index, plane) in
            frame.planes.iter_mut().enumerate().take(if alpha { 1 } else { 3 })
        {
            let mut slice = plane.mut_slice(Default::default());
            for (y, row) in slice.rows_iter_mut().take(height).enumerate() {
                for (x, output) in row[..width].iter_mut().enumerate() {
                    let p = rgba.get_pixel(x as u32, y as u32).0;
                    *output = if alpha {
                        p[3]
                    } else if p[3] == 0 {
                        if plane_index == 0 { 0 } else { 128 }
                    } else {
                        let (r, g, b) = (f32::from(p[0]), f32::from(p[1]), f32::from(p[2]));
                        let value = match plane_index {
                            0 => 0.299 * r + 0.587 * g + 0.114 * b,
                            1 => 128.0 - 0.168736 * r - 0.331264 * g + 0.5 * b,
                            _ => 128.0 + 0.5 * r - 0.418688 * g - 0.081312 * b,
                        };
                        value.round().clamp(0.0, 255.0) as u8
                    };
                }
            }
        }
        self.context.send_frame(frame)?;
        if self.independent.is_some() {
            self.context.flush();
            self.drain(true)
        } else {
            self.drain(false)
        }
    }

    fn drain(&mut self, flushed: bool) -> Result<()> {
        loop {
            match self.context.receive_packet() {
                Ok(packet) => {
                    let expected =
                        if self.independent.is_some() { 0 } else { self.track.samples.len() as u64 };
                    ensure!(
                        packet.input_frameno == expected,
                        "AVIF encoder reordered a frame"
                    );
                    self.encoded_bytes += packet.data.len();
                    ensure!(
                        self.encoded_bytes <= MAX_ENCODED_SOURCE_BYTES,
                        "Encoded animation exceeds 32 MiB"
                    );
                    self.track.samples.push(Sample {
                        bytes: packet.data,
                        sync: packet.frame_type == FrameType::KEY,
                    });
                },
                Err(EncoderStatus::Encoded) => continue,
                Err(EncoderStatus::NeedMoreData) if !flushed => return Ok(()),
                Err(EncoderStatus::LimitReached) if flushed => return Ok(()),
                Err(e) => return Err(anyhow!("AVIF animation encoder: {e:?}")),
            }
        }
    }

    fn finish(mut self) -> Result<Track> {
        if self.independent.is_none() {
            self.context.flush();
            self.drain(true)?;
        }
        Ok(self.track)
    }
}

fn encode(
    bytes: &[u8], info: &GifInfo, width: u32, height: u32, quantizer: usize, crop: Option<CropArea>,
) -> Result<Vec<u8>> {
    let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))?;
    ensure!(decoder.icc_profile()?.is_none(), "GIF colour profiles cannot be converted safely yet");
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(info.width);
    limits.max_image_height = Some(info.height);
    limits.max_alloc = Some(192 * 1024 * 1024);
    decoder.set_limits(limits)?;
    let mut colour = Encoder::new(width, height, quantizer, false)?;
    let mut alpha = info.alpha.then(|| Encoder::new(width, height, quantizer, true)).transpose()?;
    let mut frames = 0;
    for frame in decoder.into_frames() {
        let rgba = frame?.into_buffer();
        let rgba = if let Some(crop) = crop {
            image::imageops::crop_imm(&rgba, crop.x, crop.y, crop.side, crop.side).to_image()
        } else {
            rgba
        };
        let rgba = if rgba.width() == width && rgba.height() == height {
            rgba
        } else {
            image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Triangle)
        };
        colour.feed(&rgba, false)?;
        if let Some(alpha) = &mut alpha {
            alpha.feed(&rgba, true)?;
        }
        frames += 1;
        ensure!(frames <= info.delays.len(), "GIF decoder frame count disagrees");
    }
    ensure!(frames == info.delays.len(), "GIF decoder frame count disagrees");
    let colour = colour.finish()?;
    let alpha = alpha.map(Encoder::finish).transpose()?;
    sequence::mux(width, height, colour, alpha, &info.delays, info.repetitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 1x1 transparent GIF. Kept inline to exercise untrusted parsing, without
    // creating a permanent media/screenshot fixture collection.
    const SINGLE: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x01\0\x01\0\0\x02\x02\x44\x01\0\x3b";

    #[test]
    fn reject_truncated_or_expanding_gifs_before_decoding() {
        for len in 6..SINGLE.len() {
            assert!(inspect(&SINGLE[..len]).is_err());
        }
        assert_eq!(inspect(SINGLE).unwrap().repetitions, Some(0));
        let mut huge = SINGLE.to_vec();
        huge[6..10].fill(0xff);
        assert!(inspect(&huge).is_err());
        let mut outside = SINGLE.to_vec();
        let frame = outside.iter().position(|b| *b == 0x2c).unwrap();
        outside[frame + 1] = 1;
        assert!(inspect(&outside).is_err());
        let mut repeat = SINGLE.to_vec();
        let frame = repeat.iter().position(|b| *b == 0x2c).unwrap();
        let data = repeat[frame..repeat.len() - 1].to_vec();
        repeat.pop();
        for _ in 0..MAX_FRAMES {
            repeat.extend_from_slice(&data);
        }
        repeat.push(0x3b);
        assert!(inspect(&repeat).is_err());
    }

    #[test]
    fn missing_finite_and_infinite_loop_extensions_are_distinct() {
        for (value, expected) in [(0u16, None), (1, Some(1)), (5, Some(5))] {
            let mut source = SINGLE[..19].to_vec();
            source.extend_from_slice(b"\x21\xff\x0bNETSCAPE2.0\x03\x01");
            source.extend_from_slice(&value.to_le_bytes());
            source.push(0);
            source.extend_from_slice(&SINGLE[19..]);
            assert_eq!(inspect(&source).unwrap().repetitions, expected);
        }
    }

    #[test]
    fn tiny_frames_keep_the_complete_sequence_without_changing_dimensions() {
        let mut source = SINGLE[..SINGLE.len() - 1].to_vec();
        let frame = SINGLE.iter().position(|b| *b == 0x2c).unwrap();
        source.extend_from_slice(&SINGLE[frame..]);
        let image = prepare(&source, MediaPolicy { max_bytes: 64 * 1024, max_edge: 256 }).unwrap();
        let info = super::super::inspect_avif(&image.bytes).unwrap().unwrap();
        assert_eq!((info.width, info.height), (1, 1));
        assert_eq!(info.frame_count, 2);
        assert_eq!(info.duration_ms, Some(200));
        assert_eq!(info.has_alpha, Some(true));
    }
}
