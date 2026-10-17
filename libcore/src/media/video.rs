//! Outgoing video policy. Platform adapters report media facts and execute the returned plan;
//! codec choice, size/quality limits and acceptance rules have one owner here.

use anyhow::{Result, bail, ensure};

use super::policy::VIDEO;

/// Facts read from the media itself, with rotation/crop/pixel aspect already applied to display
/// dimensions. Missing optional metadata stays unknown rather than becoming a failed encode.
#[derive(Clone, Debug, uniffi::Record)]
pub struct VideoProbe {
    pub container_mime: Option<String>,
    pub video_mime: String,
    pub video_tracks: u32,
    pub audio_tracks: u32,
    pub display_width: f64,
    pub display_height: f64,
    pub duration_us: u64,
    pub frame_rate: Option<f64>,
    pub video_bitrate: Option<u32>,
    pub audio: Option<VideoAudioProbe>,
    pub hdr: bool,
    pub dynamic_hdr: bool,
    /// ISO/IEC 23091-2 (CICP) values, not platform color-space identifiers.
    pub color_primaries: Option<u16>,
    pub color_transfer: Option<u16>,
    pub full_range: Option<bool>,
    pub bit_depth: Option<u8>,
    /// CTA-861.3 static HDR descriptor bytes when declared by the source.
    pub hdr_static_metadata: Option<Vec<u8>>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct VideoAudioProbe {
    pub mime: String,
    pub duration_us: Option<u64>,
    pub bitrate: Option<u32>,
    pub channels: Option<u32>,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct VideoEncodingPlan {
    pub width: u32,
    pub height: u32,
    pub max_frame_rate: u32,
    pub video_bitrate: u32,
    pub audio_bitrate: u32,
    pub video_mime: String,
    pub audio_mime: String,
    pub keep_hdr: bool,
}

const MAX_LONG_EDGE: u32 = VIDEO.max_width;
const MAX_SHORT_EDGE: u32 = VIDEO.max_height;
const MAX_FRAME_RATE: u32 = VIDEO.max_fps;
const VIDEO_BITRATE: u32 = VIDEO.video_bitrate;
const AUDIO_BITRATE: u32 = 96_000;

pub(crate) fn plan(input: &VideoProbe, source_bytes: u64) -> Result<Option<VideoEncodingPlan>> {
    let mut candidates = plans(input, source_bytes)?;
    Ok(Some(candidates.remove(0)))
}

pub(crate) fn plans(input: &VideoProbe, source_bytes: u64) -> Result<Vec<VideoEncodingPlan>> {
    validate_probe(input)?;
    ensure!(source_bytes > 0, "The selected video is empty.");
    let mut candidates = Vec::new();
    for (width, height, fps, bitrate) in [
        (MAX_LONG_EDGE, MAX_SHORT_EDGE, MAX_FRAME_RATE, VIDEO_BITRATE),
        (MAX_LONG_EDGE, MAX_SHORT_EDGE, MAX_FRAME_RATE, 1_500_000),
        (MAX_LONG_EDGE, MAX_SHORT_EDGE, MAX_FRAME_RATE, 1_000_000),
        (960, 540, MAX_FRAME_RATE, 1_000_000),
        (960, 540, 24, 800_000),
        (VIDEO.emergency_width, VIDEO.emergency_height, VIDEO.emergency_fps, VIDEO.emergency_video_bitrate),
    ] {
        let (width, height) = fit(input.display_width, input.display_height, width, height);
        let candidate = VideoEncodingPlan {
            width,
            height,
            max_frame_rate: fps,
            video_bitrate: bitrate,
            audio_bitrate: AUDIO_BITRATE,
            video_mime: "video/avc".into(),
            audio_mime: "audio/mp4a-latm".into(),
            keep_hdr: false,
        };
        if candidates.last().is_none_or(|previous: &VideoEncodingPlan| {
            previous.width != candidate.width
                || previous.height != candidate.height
                || previous.max_frame_rate != candidate.max_frame_rate
                || previous.video_bitrate != candidate.video_bitrate
        }) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn fit(width: f64, height: f64, max_width: u32, max_height: u32) -> (u32, u32) {
    let scale = 1.0_f64.min(f64::from(max_width) / width).min(f64::from(max_height) / height);
    let even = |edge: f64| ((edge * scale) as u32 / 2 * 2).max(2);
    (even(width), even(height))
}

pub(crate) fn validate_output(
    input: &VideoProbe, output: &VideoProbe, plan: &VideoEncodingPlan,
) -> Result<()> {
    validate_probe(output)?;
    ensure!(
        is_mp4(output) && output.video_mime == plan.video_mime,
        "The compressed video has an unsupported format."
    );

    // Media3 asks the codec for its closest supported resolution. For example a 1080×2340
    // picture fits 590×1280, while an encoder aligned to 16 pixels can produce 592×1280.
    // The old target+1 comparison rejected that valid result. Allow one alignment block's
    // rounding while still enforcing our output ceiling and rejecting changed orientation.
    const ALIGNMENT_ROUNDING: f64 = 15.0;
    ensure!(
        output.display_width <= f64::from(plan.width) + ALIGNMENT_ROUNDING
            && output.display_height <= f64::from(plan.height) + ALIGNMENT_ROUNDING
            && output.display_width.max(output.display_height) <= f64::from(MAX_LONG_EDGE)
            && output.display_width.min(output.display_height) <= f64::from(MAX_SHORT_EDGE),
        "The encoder produced a larger video than requested."
    );
    let aspect_error = (output.display_width * input.display_height
        - output.display_height * input.display_width)
        .abs();
    ensure!(
        aspect_error <= input.display_width.max(input.display_height) * ALIGNMENT_ROUNDING,
        "The encoder changed the video's proportions."
    );

    // Compare the presentation timeline, not a raw track-header duration which can include
    // portions hidden by MP4 edit lists. The adapter uses the same parser as the encoder.
    ensure!(
        output.duration_us.abs_diff(input.duration_us) <= 250_000,
        "The compressed video's duration does not match the original."
    );
    if let Some(fps) = output.frame_rate {
        // Container readers can infer FPS from sample count divided by duration. A short
        // clip's last frame and integer rounding can exceed the nominal rate slightly.
        let rounding = (1_000_000.0 / output.duration_us as f64).max(0.5);
        ensure!(
            fps <= f64::from(plan.max_frame_rate) + rounding,
            "The encoder did not reduce the video's frame rate."
        );
    }
    match (&input.audio, &output.audio) {
        (Some(before), Some(after)) => {
            ensure!(
                after.mime == plan.audio_mime,
                "The compressed video's audio format is unsupported."
            );
            if let (Some(original), Some(encoded)) = (before.channels, after.channels) {
                ensure!(original == encoded, "The encoder changed the video's audio channels.");
            }
            // These durations are optional presentation durations, never unadjusted mdhd values.
            if let (Some(original), Some(encoded)) = (before.duration_us, after.duration_us) {
                ensure!(
                    original.abs_diff(encoded) <= 250_000,
                    "The compressed video's audio duration does not match the original."
                );
            }
        },
        (None, None) => {},
        (Some(_), None) => bail!("The compressed video is missing its audio."),
        (None, Some(_)) => bail!("The encoder added an unexpected audio track."),
    }
    ensure!(!output.hdr, "The encoder did not tone-map the video to SDR.");
    ensure!(
        output.bit_depth.is_none_or(|depth| depth == 8),
        "The encoder did not produce eight-bit video."
    );
    Ok(())
}

fn is_mp4(probe: &VideoProbe) -> bool {
    matches!(probe.container_mime.as_deref(), Some("video/mp4" | "application/mp4"))
}

fn validate_probe(probe: &VideoProbe) -> Result<()> {
    ensure!(probe.video_tracks == 1 && probe.audio_tracks <= 1,
        "This video's tracks are not supported for compression.");
    ensure!((probe.audio_tracks == 1) == probe.audio.is_some(),
        "The video's audio details could not be read.");
    ensure!(
        probe.display_width.is_finite()
            && probe.display_height.is_finite()
            && (2.0..=1_638_400.0).contains(&probe.display_width)
            && (2.0..=1_638_400.0).contains(&probe.display_height),
        "The video's dimensions could not be read."
    );
    ensure!(probe.duration_us > 0, "The video's duration could not be read.");
    ensure!(
        probe.frame_rate.is_none_or(|fps| fps.is_finite() && fps > 0.0),
        "The video's frame rate could not be read."
    );
    ensure!(
        probe.video_bitrate != Some(0) && probe.audio.as_ref().is_none_or(|a| a.bitrate != Some(0)),
        "The video's bitrate could not be read."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> VideoProbe {
        VideoProbe {
            container_mime: Some("video/mp4".into()),
            video_mime: "video/avc".into(),
            video_tracks: 1,
            audio_tracks: 0,
            display_width: 1080.0,
            display_height: 2340.0,
            duration_us: 10_000_000,
            frame_rate: Some(30.0),
            video_bitrate: Some(8_000_000),
            audio: None,
            hdr: false,
            dynamic_hdr: false,
            color_primaries: Some(1),
            color_transfer: Some(1),
            full_range: Some(false),
            bit_depth: Some(8),
            hdr_static_metadata: None,
        }
    }

    #[test]
    fn portrait_codec_alignment_is_accepted_but_rotation_and_oversizing_are_not() {
        let source = source();
        let plan = plan(&source, 10_000_000).unwrap().unwrap();
        assert_eq!((plan.width, plan.height), (332, 720));
        let mut encoded = source.clone();
        encoded.display_width = 336.0;
        encoded.display_height = 720.0;
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.display_width = 720.0;
        encoded.display_height = 336.0;
        assert!(validate_output(&source, &encoded, &plan).is_err());
        encoded.display_width = 1080.0;
        encoded.display_height = 2340.0;
        assert!(validate_output(&source, &encoded, &plan).is_err());
    }

    #[test]
    fn short_clip_frame_rate_rounding_does_not_accept_a_60_fps_output() {
        let mut source = source();
        source.duration_us = 1_000_000;
        let plan = plan(&source, 1_000_000).unwrap().unwrap();
        let mut encoded = source.clone();
        encoded.display_width = 336.0;
        encoded.display_height = 720.0;
        encoded.frame_rate = Some(31.0);
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.frame_rate = Some(60.0);
        assert!(validate_output(&source, &encoded, &plan).is_err());
        encoded.frame_rate = Some(30.0);
        encoded.duration_us = 400_000;
        assert!(validate_output(&source, &encoded, &plan).is_err());
    }

    #[test]
    fn unknown_optional_audio_metadata_is_not_a_failed_encode_but_missing_audio_is() {
        let mut source = source();
        source.audio = Some(VideoAudioProbe {
            mime: "audio/mp4a-latm".into(),
            duration_us: None,
            bitrate: Some(192_000),
            channels: Some(2),
        });
        source.audio_tracks = 1;
        let plan = plan(&source, 10_000_000).unwrap().unwrap();
        let mut encoded = source.clone();
        encoded.display_width = 336.0;
        encoded.display_height = 720.0;
        encoded.audio.as_mut().unwrap().channels = None;
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.audio = None;
        encoded.audio_tracks = 0;
        assert_eq!(
            validate_output(&source, &encoded, &plan).unwrap_err().to_string(),
            "The compressed video is missing its audio."
        );
    }

    #[test]
    fn hdr_input_is_tone_mapped_to_eight_bit_sdr() {
        let mut source = source();
        source.hdr = true;
        source.video_mime = "video/hevc".into();
        source.color_transfer = Some(16);
        source.color_primaries = Some(9);
        source.bit_depth = Some(10);
        source.hdr_static_metadata = Some(vec![0, 1, 2, 3]);
        let plan = plan(&source, 10_000_000).unwrap().unwrap();
        let mut encoded = source.clone();
        encoded.video_mime = "video/avc".into();
        encoded.display_width = 336.0;
        encoded.display_height = 720.0;
        encoded.hdr = false;
        encoded.bit_depth = Some(8);
        encoded.color_transfer = Some(1);
        encoded.color_primaries = Some(1);
        encoded.hdr_static_metadata = None;
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.hdr = true;
        assert!(validate_output(&source, &encoded, &plan).is_err());
        encoded.hdr = false;
        encoded.bit_depth = Some(10);
        assert!(validate_output(&source, &encoded, &plan).is_err());
    }

    #[test]
    fn plans_are_ordered_from_target_to_emergency() {
        let plans = plans(&source(), 10_000_000).unwrap();
        assert_eq!(plans.first().unwrap().video_bitrate, VIDEO_BITRATE);
        assert_eq!(plans.last().unwrap().video_bitrate, VIDEO.emergency_video_bitrate);
        assert!(plans.iter().all(|plan| !plan.keep_hdr && plan.video_mime == "video/avc"));
    }
}
