use std::{path::PathBuf, sync::{Arc, atomic::{AtomicBool, Ordering}}};

use anyhow::{Result, bail};

use super::{MediaRequest, image, import::ImportedSource, ports::SourceLease, video};

/// A media import is consumed exactly once. Its private source disappears when
/// the job completes, is cancelled, or is dropped.
pub(crate) struct MediaJob {
    source: Option<JobSource>,
    request: MediaRequest,
    import_directory: PathBuf,
    cancellation: MediaCancellation,
}

#[derive(Clone, Default)]
pub(crate) struct MediaCancellation {
    cancelled: Arc<AtomicBool>,
    on_cancel: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl MediaCancellation {
    pub(crate) fn with_callback(on_cancel: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { cancelled: Arc::new(AtomicBool::new(false)), on_cancel: Some(on_cancel) }
    }

    pub(crate) fn cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            if let Some(on_cancel) = &self.on_cancel { on_cancel() }
        }
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) { bail!("Media preparation was cancelled.") }
        Ok(())
    }
}

enum JobSource {
    Lease(SourceLease),
}

pub(crate) enum PreparedMedia {
    Image(image::PreparedImage),
    Video(video::PreparedVideo),
}

pub(crate) struct StagedImage {
    pub(crate) id: u64,
    pub(crate) avif: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) animated: bool,
}

pub(crate) struct StagedVideo {
    pub(crate) id: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) poster: Vec<u8>,
}

impl MediaJob {
    pub(crate) fn from_source_with_cancellation(
        source: SourceLease, request: MediaRequest, import_directory: PathBuf,
        cancellation: MediaCancellation,
    ) -> Result<Self> {
        request.validate().map_err(|error| anyhow::anyhow!(error))?;
        Ok(Self {
            source: Some(JobSource::Lease(source)), request, import_directory,
            cancellation,
        })
    }

    pub(crate) fn cancellation(&self) -> MediaCancellation { self.cancellation.clone() }

    pub(crate) fn cancel(&mut self) {
        self.cancellation.cancel();
        self.source.take();
    }

    pub(crate) fn run(&mut self) -> Result<PreparedMedia> {
        self.cancellation.check()?;
        let source =
            self.source.take().ok_or_else(|| anyhow::anyhow!("This media job is finished."))?;
        let JobSource::Lease(mut source) = source;
        source.set_cancellation(self.cancellation.clone());
        if matches!(self.request, MediaRequest::ChatVideo { .. }) {
            let MediaRequest::ChatVideo { trim } = self.request else { unreachable!() };
            return Ok(PreparedMedia::Video(video::prepare_video(
                &mut source,
                &self.import_directory,
                trim, &self.cancellation,
            )?));
        }
        let header = source.read_chunk(0, 64)?;
        let source = match image::inspect(&header)? {
            image::ImageKind::Platform => {
                if source.inspect_image().is_ok() {
                    let image = image::prepare_platform_image(&mut source, self.request)?;
                    self.cancellation.check()?;
                    return Ok(PreparedMedia::Image(image));
                }
                if matches!(self.request, MediaRequest::Sticker { .. }) {
                    let MediaRequest::Sticker { trim } = self.request else { unreachable!() };
                    return Ok(PreparedMedia::Image(video::prepare_video_sticker(
                        &mut source,
                        trim,
                        &self.cancellation,
                    )?));
                }
                bail!("This image format is not supported on this device.");
            },
            _ => ImportedSource::from_lease(source, &self.import_directory)?,
        };
        self.cancellation.check()?;
        let prepared = match self.request {
            MediaRequest::ChatImage { .. }
            | MediaRequest::Avatar { .. }
            | MediaRequest::Sticker { .. } => {
                PreparedMedia::Image(image::prepare_image_path(source.path(), self.request)?)
            },
            MediaRequest::ChatVideo { .. } => unreachable!("handled before image import"),
        };
        self.cancellation.check()?;
        Ok(prepared)
    }

    pub(crate) fn stage(&mut self) -> Result<StagedImage> {
        if !matches!(self.request, MediaRequest::ChatImage { .. }) {
            bail!("Only chat images can be staged as message media.");
        }
        match self.run()? {
            PreparedMedia::Image(image) => {
                let staged = StagedImage {
                    avif: image.bytes.clone(),
                    width: image.width,
                    height: image.height,
                    animated: image.animated,
                    id: crate::staging::stage_prepared_image(image),
                };
                Ok(staged)
            },
            PreparedMedia::Video(_) => unreachable!("chat image request cannot prepare video"),
        }
    }

    pub(crate) fn stage_video(&mut self) -> Result<StagedVideo> {
        if !matches!(self.request, MediaRequest::ChatVideo { .. }) {
            bail!("Only chat videos can be staged as video media.");
        }
        let PreparedMedia::Video(mut video) = self.run()? else {
            unreachable!("chat video request cannot prepare an image")
        };
        let poster = video.poster.bytes.clone();
        let id = crate::staging::stage_prepared_video(&mut video)?;
        Ok(StagedVideo { id, width: video.width, height: video.height, poster })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

    use ::image::ImageEncoder;
    use anyhow::Result;

    use super::*;
    use crate::media::{
        frames::PixelFormat,
        ports::{DecodedImageFrame, ImageFrameRequest, ImageProbe, MediaSource, VideoFrameRequest},
        video::VideoProbe,
    };

    struct Source {
        bytes: Vec<u8>,
        released: Arc<Mutex<u32>>,
    }

    impl MediaSource for Source {
        fn length(&self) -> Result<Option<u64>> {
            Ok(Some(self.bytes.len() as u64))
        }

        fn read(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
            let start = usize::try_from(offset)?;
            let end = (start + max_bytes as usize).min(self.bytes.len());
            Ok(self.bytes[start..end].to_vec())
        }

        fn release(&mut self) {
            *self.released.lock().unwrap() += 1;
        }
    }

    struct PlatformSource {
        released: Arc<Mutex<u32>>,
    }

    impl MediaSource for PlatformSource {
        fn length(&self) -> Result<Option<u64>> { Ok(Some(24)) }

        fn read(&mut self, _offset: u64, _max_bytes: u32) -> Result<Vec<u8>> {
            Ok(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec())
        }

        fn release(&mut self) {
            *self.released.lock().unwrap() += 1;
        }

        fn inspect_image(&mut self) -> Result<ImageProbe> {
            Ok(ImageProbe { width: 1, height: 1, animated: false, source_is_hdr: false })
        }

        fn decode_image(&mut self, request: ImageFrameRequest) -> Result<DecodedImageFrame> {
            assert_eq!(request.max_edge, 1);
            Ok(DecodedImageFrame {
                pixels: vec![1, 2, 3, 4],
                width: 1,
                height: 1,
                format: PixelFormat::Rgba8,
                source_is_hdr: false,
            })
        }
    }

    #[test]
    fn cancel_releases_the_unread_source_lease() {
        let directory = tempfile::tempdir().unwrap();
        let released = Arc::new(Mutex::new(0));
        let source = SourceLease::open(Box::new(Source { bytes: vec![1], released: released.clone() })).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            source, MediaRequest::ChatImage { hd_enabled: false }, directory.path().into(),
            MediaCancellation::default(),
        ).unwrap();
        job.cancel();
        assert_eq!(*released.lock().unwrap(), 1);
        assert!(job.run().is_err());
    }

    #[test]
    fn shared_cancellation_prevents_a_job_from_consuming_or_staging_its_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = SourceLease::open(Box::new(Source {
            bytes: vec![1], released: Arc::new(Mutex::new(0)),
        })).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            source, MediaRequest::ChatImage { hd_enabled: false }, directory.path().into(),
            MediaCancellation::default(),
        ).unwrap();
        job.cancellation().cancel();
        let Err(error) = job.run() else { panic!("cancelled job ran") };
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn cancellation_notifies_the_platform_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let cancellation = MediaCancellation::with_callback({
            let calls = calls.clone();
            Arc::new(move || { calls.fetch_add(1, Ordering::Relaxed); })
        });
        cancellation.cancel();
        cancellation.cancel();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(cancellation.check().is_err());
    }

    #[test]
    fn job_owns_and_normalizes_a_private_still() {
        let directory = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[1, 2, 3, 4], 1, 1, ::image::ColorType::Rgba8.into())
            .unwrap();
        let source = SourceLease::open(Box::new(Source {
            bytes: png, released: Arc::new(Mutex::new(0)),
        })).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            source, MediaRequest::ChatImage { hd_enabled: false }, directory.path().into(),
            MediaCancellation::default(),
        ).unwrap();
        let PreparedMedia::Image(image) = job.run().unwrap() else { unreachable!() };
        assert_eq!(image::inspect(&image.bytes).unwrap(), image::ImageKind::Avif);
    }

    #[test]
    fn platform_image_decode_uses_the_opaque_lease_until_the_job_finishes() {
        let releases = Arc::new(Mutex::new(0));
        let lease =
            SourceLease::open(Box::new(PlatformSource { released: releases.clone() })).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            lease,
            MediaRequest::ChatImage { hd_enabled: false },
            tempfile::tempdir().unwrap().keep(),
            MediaCancellation::default(),
        )
        .unwrap();
        let PreparedMedia::Image(image) = job.run().unwrap() else { unreachable!() };
        assert_eq!(image::inspect(&image.bytes).unwrap(), image::ImageKind::Avif);
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn cancelling_an_unread_platform_source_releases_its_lease() {
        let releases = Arc::new(Mutex::new(0));
        let lease =
            SourceLease::open(Box::new(PlatformSource { released: releases.clone() })).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            lease,
            MediaRequest::ChatImage { hd_enabled: false },
            tempfile::tempdir().unwrap().keep(),
            MediaCancellation::default(),
        )
        .unwrap();
        job.cancel();
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    struct VideoStickerSource;

    impl MediaSource for VideoStickerSource {
        fn length(&self) -> Result<Option<u64>> {
            Ok(Some(24))
        }

        fn read(&mut self, _offset: u64, _max_bytes: u32) -> Result<Vec<u8>> {
            Ok(b"\0\0\0\x18ftypmp42\0\0\0\0isommp42".to_vec())
        }

        fn release(&mut self) {}

        fn inspect_video(&mut self) -> Result<VideoProbe> {
            Ok(VideoProbe {
                container_mime: Some("video/mp4".into()), video_mime: "video/avc".into(),
                video_tracks: 1, audio_tracks: 0, display_width: 32.0, display_height: 32.0,
                duration_us: 100_000, frame_rate: Some(30.0), video_bitrate: Some(1_000),
                audio: None, hdr: false, dynamic_hdr: false, color_primaries: Some(1),
                color_transfer: Some(1), full_range: Some(false), bit_depth: Some(8), hdr_static_metadata: None,
            })
        }

        fn open_video_frame(&mut self, request: VideoFrameRequest) -> Result<DecodedImageFrame> {
            Ok(DecodedImageFrame {
                pixels: vec![request.timestamp_us as u8, 0, 0, 255].repeat(32 * 32),
                width: 32, height: 32, format: PixelFormat::Rgba8, source_is_hdr: false,
            })
        }
    }

    #[test]
    fn sticker_job_routes_video_sources_through_the_animated_image_pipeline() {
        let lease = SourceLease::open(Box::new(VideoStickerSource)).unwrap();
        let mut job = MediaJob::from_source_with_cancellation(
            lease, MediaRequest::Sticker { trim: None }, tempfile::tempdir().unwrap().keep(),
            MediaCancellation::default(),
        )
        .unwrap();
        let PreparedMedia::Image(sticker) = job.run().unwrap() else { unreachable!() };
        assert!(sticker.animated);
        assert_eq!(image::inspect(&sticker.bytes).unwrap(), image::ImageKind::Avif);
    }
}
