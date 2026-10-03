//! Outgoing video policy. Platform adapters report media facts and execute the returned plan;
//! codec choice, size/quality limits and acceptance rules have one owner here.

use anyhow::{Result, bail, ensure};

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

const MAX_LONG_EDGE: u32 = 1280;
const MAX_SHORT_EDGE: u32 = 720;
const MAX_FRAME_RATE: u32 = 30;
const VIDEO_BITRATE: u32 = 1_500_000;
const AUDIO_BITRATE: u32 = 96_000;

pub(crate) fn plan(input: &VideoProbe, source_bytes: u64) -> Result<Option<VideoEncodingPlan>> {
    validate_probe(input)?;
    ensure!(source_bytes > 0, "The selected video is empty.");
    let long = input.display_width.max(input.display_height);
    let short = input.display_width.min(input.display_height);
    let overall_bitrate = source_bytes as f64 * 8_000_000.0 / input.duration_us as f64;
    let video_bitrate = input.video_bitrate.map(f64::from).unwrap_or(overall_bitrate);
    let efficient_codec = if input.hdr {
        matches!(input.video_mime.as_str(), "video/hevc" | "video/dolby-vision")
    } else {
        input.video_mime == "video/avc"
    };
    let efficient_audio = input.audio.as_ref().is_none_or(|audio| {
        audio.mime == "audio/mp4a-latm" && audio.bitrate.is_some_and(|b| b <= AUDIO_BITRATE)
    });
    if is_mp4(input)
        && efficient_codec
        && efficient_audio
        && long <= f64::from(MAX_LONG_EDGE)
        && short <= f64::from(MAX_SHORT_EDGE)
        && input.frame_rate.is_some_and(|fps| fps <= f64::from(MAX_FRAME_RATE))
        && video_bitrate <= f64::from(VIDEO_BITRATE)
    {
        return Ok(None);
    }
    ensure!(
        !input.dynamic_hdr,
        "This HDR video cannot be compressed without losing its HDR metadata."
    );
    if input.hdr {
        ensure!(
            matches!(input.color_transfer, Some(16 | 18)) && input.color_primaries.is_some(),
            "The video's HDR format could not be identified."
        );
    }
    let scale = 1.0_f64.min(f64::from(MAX_LONG_EDGE) / long).min(f64::from(MAX_SHORT_EDGE) / short);
    // This is the requested picture box. Hardware encoders may align it slightly; that is
    // checked separately from a real resize/aspect error when accepting the output.
    let even = |edge: f64| ((edge * scale) as u32 / 2 * 2).max(2);
    Ok(Some(VideoEncodingPlan {
        width: even(input.display_width),
        height: even(input.display_height),
        max_frame_rate: MAX_FRAME_RATE,
        video_bitrate: input.video_bitrate.unwrap_or(VIDEO_BITRATE).min(VIDEO_BITRATE),
        audio_bitrate: input
            .audio
            .as_ref()
            .and_then(|a| a.bitrate)
            .unwrap_or(AUDIO_BITRATE)
            .min(AUDIO_BITRATE),
        video_mime: if input.hdr { "video/hevc" } else { "video/avc" }.into(),
        audio_mime: "audio/mp4a-latm".into(),
        keep_hdr: input.hdr,
    }))
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
    if input.hdr {
        ensure!(
            output.hdr
                && output.bit_depth.is_some_and(|depth| depth >= 10)
                && output.color_transfer == input.color_transfer
                && output.color_primaries == input.color_primaries
                && input
                    .hdr_static_metadata
                    .as_ref()
                    .is_none_or(|m| Some(m) == output.hdr_static_metadata.as_ref()),
            "The compressed video did not preserve its HDR data."
        );
    } else {
        ensure!(!output.hdr, "The encoder unexpectedly changed the video to HDR.");
    }
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
        assert_eq!((plan.width, plan.height), (590, 1280));
        let mut encoded = source.clone();
        encoded.display_width = 592.0;
        encoded.display_height = 1280.0;
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.display_width = 1280.0;
        encoded.display_height = 592.0;
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
        encoded.display_width = 590.0;
        encoded.display_height = 1280.0;
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
        encoded.display_width = 590.0;
        encoded.display_height = 1280.0;
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
    fn hdr_output_must_retain_precision_color_and_declared_metadata() {
        let mut source = source();
        source.hdr = true;
        source.video_mime = "video/hevc".into();
        source.color_transfer = Some(16);
        source.color_primaries = Some(9);
        source.bit_depth = Some(10);
        source.hdr_static_metadata = Some(vec![0, 1, 2, 3]);
        let plan = plan(&source, 10_000_000).unwrap().unwrap();
        let mut encoded = source.clone();
        encoded.display_width = 590.0;
        encoded.display_height = 1280.0;
        validate_output(&source, &encoded, &plan).unwrap();
        encoded.bit_depth = Some(8);
        assert!(validate_output(&source, &encoded, &plan).is_err());
        encoded.bit_depth = Some(10);
        encoded.hdr_static_metadata = None;
        assert!(validate_output(&source, &encoded, &plan).is_err());
    }
}
