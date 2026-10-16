use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{MediaRequest, image, import::ImportedSource, ports::SourceLease};

/// A media import is consumed exactly once. Its private source disappears when
/// the job completes, is cancelled, or is dropped.
pub struct MediaJob {
    source: Option<JobSource>,
    request: MediaRequest,
    import_directory: PathBuf,
}

enum JobSource {
    Lease(SourceLease),
    Private(ImportedSource),
}

pub enum PreparedMedia {
    Image(image::PreparedImage),
}

pub struct StagedImage {
    pub id: u64,
    pub avif: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub animated: bool,
}

impl MediaJob {
    pub fn new(source: ImportedSource, request: MediaRequest) -> Result<Self> {
        request.validate().map_err(|error| anyhow::anyhow!(error))?;
        Ok(Self {
            source: Some(JobSource::Private(source)),
            request,
            import_directory: PathBuf::new(),
        })
    }

    pub fn from_source(
        source: SourceLease, request: MediaRequest, import_directory: PathBuf,
    ) -> Result<Self> {
        request.validate().map_err(|error| anyhow::anyhow!(error))?;
        Ok(Self { source: Some(JobSource::Lease(source)), request, import_directory })
    }

    pub fn cancel(&mut self) {
        self.source.take();
    }

    pub fn run(&mut self) -> Result<PreparedMedia> {
        let source =
            self.source.take().ok_or_else(|| anyhow::anyhow!("This media job is finished."))?;
        let source = match source {
            JobSource::Private(source) => source,
            JobSource::Lease(mut source) => {
                let header = source.read_chunk(0, 64)?;
                match image::inspect(&header)? {
                    image::ImageKind::Platform => {
                        return Ok(PreparedMedia::Image(image::prepare_platform_image(
                            &mut source,
                            self.request,
                        )?));
                    },
                    _ => ImportedSource::from_lease(source, &self.import_directory)?,
                }
            },
        };
        match self.request {
            MediaRequest::ChatImage { .. }
            | MediaRequest::Avatar { .. }
            | MediaRequest::Sticker { .. } => {
                Ok(PreparedMedia::Image(image::prepare_image_path(source.path(), self.request)?))
            },
            MediaRequest::ChatVideo { .. } => bail!("Video preparation has not been configured."),
        }
    }

    pub fn stage(&mut self) -> Result<StagedImage> {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ::image::ImageEncoder;
    use anyhow::Result;

    use super::*;
    use crate::media::{
        frames::PixelFormat,
        ports::{DecodedImageFrame, ImageFrameRequest, ImageProbe, MediaSource},
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
        fn length(&self) -> Result<Option<u64>> {
            Ok(Some(24))
        }

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
    fn cancel_releases_the_consumed_private_import() {
        let directory = tempfile::tempdir().unwrap();
        let released = Arc::new(Mutex::new(0));
        let source = ImportedSource::from_source(
            Box::new(Source { bytes: vec![1], released: released.clone() }),
            directory.path(),
        )
        .unwrap();
        let path = source.path().to_owned();
        let mut job = MediaJob::new(source, MediaRequest::ChatImage { hd_enabled: false }).unwrap();
        assert_eq!(*released.lock().unwrap(), 1);
        job.cancel();
        assert!(!path.exists());
        assert!(job.run().is_err());
    }

    #[test]
    fn job_owns_and_normalizes_a_private_still() {
        let directory = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        ::image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[1, 2, 3, 4], 1, 1, ::image::ColorType::Rgba8.into())
            .unwrap();
        let source = ImportedSource::from_source(
            Box::new(Source { bytes: png, released: Arc::new(Mutex::new(0)) }),
            directory.path(),
        )
        .unwrap();
        let path = source.path().to_owned();
        let mut job = MediaJob::new(source, MediaRequest::ChatImage { hd_enabled: false }).unwrap();
        let PreparedMedia::Image(image) = job.run().unwrap();
        assert_eq!(image::inspect(&image.bytes).unwrap(), image::ImageKind::Avif);
        assert!(!path.exists());
    }

    #[test]
    fn platform_image_decode_uses_the_opaque_lease_until_the_job_finishes() {
        let releases = Arc::new(Mutex::new(0));
        let lease =
            SourceLease::open(Box::new(PlatformSource { released: releases.clone() })).unwrap();
        let mut job = MediaJob::from_source(
            lease,
            MediaRequest::ChatImage { hd_enabled: false },
            tempfile::tempdir().unwrap().keep(),
        )
        .unwrap();
        let PreparedMedia::Image(image) = job.run().unwrap();
        assert_eq!(image::inspect(&image.bytes).unwrap(), image::ImageKind::Avif);
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn cancelling_an_unread_platform_source_releases_its_lease() {
        let releases = Arc::new(Mutex::new(0));
        let lease =
            SourceLease::open(Box::new(PlatformSource { released: releases.clone() })).unwrap();
        let mut job = MediaJob::from_source(
            lease,
            MediaRequest::ChatImage { hd_enabled: false },
            tempfile::tempdir().unwrap().keep(),
        )
        .unwrap();
        job.cancel();
        assert_eq!(*releases.lock().unwrap(), 1);
    }
}
