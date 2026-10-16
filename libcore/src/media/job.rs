use anyhow::{Result, bail};

use super::{MediaRequest, image, import::ImportedSource};

/// A media import is consumed exactly once. Its private source disappears when
/// the job completes, is cancelled, or is dropped.
pub struct MediaJob {
    source: Option<ImportedSource>,
    request: MediaRequest,
}

pub enum PreparedMedia {
    Image(image::PreparedImage),
}

impl MediaJob {
    pub fn new(source: ImportedSource, request: MediaRequest) -> Result<Self> {
        request.validate().map_err(|error| anyhow::anyhow!(error))?;
        Ok(Self { source: Some(source), request })
    }

    pub fn cancel(&mut self) {
        self.source.take();
    }

    pub fn run(&mut self) -> Result<PreparedMedia> {
        let source =
            self.source.take().ok_or_else(|| anyhow::anyhow!("This media job is finished."))?;
        match self.request {
            MediaRequest::ChatImage { .. }
            | MediaRequest::Avatar { .. }
            | MediaRequest::Sticker { .. } => {
                Ok(PreparedMedia::Image(image::prepare_image_path(source.path(), self.request)?))
            },
            MediaRequest::ChatVideo { .. } => bail!("Video preparation has not been configured."),
        }
    }

    pub fn stage(&mut self) -> Result<u64> {
        match self.run()? {
            PreparedMedia::Image(image) => Ok(crate::staging::stage_prepared_image(image)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ::image::ImageEncoder;
    use anyhow::Result;

    use super::*;
    use crate::media::ports::MediaSource;

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
}
