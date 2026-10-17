use std::{fs::File, path::PathBuf};
use std::io::Write;

use anyhow::{Result, bail, ensure};

use super::{
    frames::PixelFormat,
    policy::MAX_SOURCE_BYTES,
    video::{VideoEncodingPlan, VideoProbe},
};

pub const READ_CHUNK_BYTES: u32 = 1024 * 1024;

/// The platform receives a core-selected decode target, never a quality or
/// output-format decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageFrameRequest {
    pub max_edge: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageProbe {
    pub width: u32,
    pub height: u32,
    pub animated: bool,
    pub source_is_hdr: bool,
}

/// A single oriented platform-decoded frame. Animated platform formats fall
/// back to their first displayed frame until their frame source is available.
pub struct DecodedImageFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub source_is_hdr: bool,
}

/// Core owns the requested rendition and output lease. The adapter receives no
/// source path: it renders from the source lease it already owns.
#[derive(Clone, Debug)]
pub struct VideoRenderRequest {
    pub plan: VideoEncodingPlan,
    pub output: PathBuf,
}

/// The adapter must inspect the MP4 it just wrote and decode its displayed
/// poster frame. Core validates both before either artifact can be staged.
pub struct VideoRenderReport {
    pub output: VideoProbe,
    pub poster: DecodedImageFrame,
    pub audio_removed: bool,
}

pub trait MediaSource: Send {
    /// A known length lets core refuse an over-limit source before reading it.
    /// Providers that cannot know it stream until EOF under the same hard cap.
    fn length(&self) -> Result<Option<u64>>;
    fn read(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>>;
    fn release(&mut self);

    fn inspect_image(&mut self) -> Result<ImageProbe> {
        bail!("This image format needs a platform decoder.")
    }

    fn decode_image(&mut self, _request: ImageFrameRequest) -> Result<DecodedImageFrame> {
        bail!("This image format needs a platform decoder.")
    }

    fn inspect_video(&mut self) -> Result<VideoProbe> {
        bail!("This video format needs a platform decoder.")
    }

    fn render_video(&mut self, _request: VideoRenderRequest) -> Result<VideoRenderReport> {
        bail!("This device cannot prepare video.")
    }
}

pub struct SourceLease {
    source: Option<Box<dyn MediaSource>>,
    length: Option<u64>,
}

impl SourceLease {
    pub fn open(source: Box<dyn MediaSource>) -> Result<Self> {
        let length = source.length()?;
        if let Some(length) = length {
            ensure!(length > 0, "The selected file is empty.");
            ensure!(
                length <= MAX_SOURCE_BYTES,
                "The selected file exceeds the 100 MiB import limit."
            );
        }
        Ok(Self { source: Some(source), length })
    }

    pub fn read_chunk(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
        if let Some(length) = self.length {
            ensure!(offset <= length, "source offset is out of range");
        }
        ensure!(max_bytes <= READ_CHUNK_BYTES, "source read exceeds the 1 MiB adapter limit");
        let requested = self
            .length
            .map_or(u64::from(max_bytes), |length| length - offset)
            .min(u64::from(max_bytes));
        let bytes =
            self.source.as_deref_mut().expect("released source").read(offset, requested as u32)?;
        ensure!(
            u64::try_from(bytes.len())? <= requested,
            "source returned more bytes than requested"
        );
        Ok(bytes)
    }

    /// Copies the source in bounded reads. `path` must be a new private file.
    pub fn copy_to(&mut self, file: &mut File) -> Result<u64> {
        let mut copied = 0_u64;
        loop {
            if self.length == Some(copied) {
                break;
            }
            if self.length.is_none() && copied == MAX_SOURCE_BYTES {
                ensure!(
                    self.read_chunk(copied, 1)?.is_empty(),
                    "The selected file exceeds the 100 MiB import limit."
                );
                break;
            }
            let remaining = MAX_SOURCE_BYTES - copied;
            let requested = remaining.min(u64::from(READ_CHUNK_BYTES)) as u32;
            let chunk = self.read_chunk(copied, requested)?;
            if chunk.is_empty() {
                break;
            }
            let chunk_len = u64::try_from(chunk.len())?;
            ensure!(chunk_len <= remaining, "The selected file exceeds the 100 MiB import limit.");
            file.write_all(&chunk)?;
            copied += chunk_len;
        }
        ensure!(copied > 0, "The selected file is empty.");
        if let Some(length) = self.length {
            ensure!(copied == length, "source ended before its reported length");
        }
        Ok(copied)
    }

    pub fn decode_image(&mut self, request: ImageFrameRequest) -> Result<DecodedImageFrame> {
        self.source.as_deref_mut().expect("released source").decode_image(request)
    }

    pub fn inspect_image(&mut self) -> Result<ImageProbe> {
        self.source.as_deref_mut().expect("released source").inspect_image()
    }

    pub fn inspect_video(&mut self) -> Result<VideoProbe> {
        self.source.as_deref_mut().expect("released source").inspect_video()
    }

    pub fn render_video(&mut self, request: VideoRenderRequest) -> Result<VideoRenderReport> {
        self.source.as_deref_mut().expect("released source").render_video(request)
    }

    pub fn known_length(&self) -> Option<u64> {
        self.length
    }

    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(mut source) = self.source.take() {
            source.release();
        }
    }
}

impl Drop for SourceLease {
    fn drop(&mut self) {
        self.release_inner();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Source {
        bytes: Vec<u8>,
        reads: Arc<Mutex<Vec<u32>>>,
        releases: Arc<Mutex<u32>>,
    }

    impl MediaSource for Source {
        fn length(&self) -> Result<Option<u64>> {
            Ok(Some(self.bytes.len() as u64))
        }

        fn read(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
            self.reads.lock().unwrap().push(max_bytes);
            let offset = usize::try_from(offset)?;
            Ok(self.bytes[offset..offset + usize::try_from(max_bytes)?].to_vec())
        }

        fn release(&mut self) {
            *self.releases.lock().unwrap() += 1;
        }
    }

    #[test]
    fn source_copy_is_chunked_and_released_once() {
        let reads = Arc::new(Mutex::new(Vec::new()));
        let releases = Arc::new(Mutex::new(0));
        let bytes = vec![7; READ_CHUNK_BYTES as usize + 9];
        let source =
            Source { bytes: bytes.clone(), reads: reads.clone(), releases: releases.clone() };
        let mut lease = SourceLease::open(Box::new(source)).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        let mut output = File::create(&path).unwrap();
        assert_eq!(lease.copy_to(&mut output).unwrap(), bytes.len() as u64);
        drop(output);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert_eq!(*reads.lock().unwrap(), vec![READ_CHUNK_BYTES, 9]);
        lease.release();
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn source_cannot_bypass_the_import_cap() {
        let reads = Arc::new(Mutex::new(Vec::new()));
        let releases = Arc::new(Mutex::new(0));
        let source = Source { bytes: vec![0; 1], reads, releases: releases.clone() };
        let mut lease = SourceLease::open(Box::new(source)).unwrap();
        assert!(lease.read_chunk(0, READ_CHUNK_BYTES + 1).is_err());
        drop(lease);
        assert_eq!(*releases.lock().unwrap(), 1);
    }

    #[test]
    fn unknown_length_stops_at_the_import_cap() {
        struct Endless;

        impl MediaSource for Endless {
            fn length(&self) -> Result<Option<u64>> {
                Ok(None)
            }

            fn read(&mut self, _offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
                Ok(vec![0; max_bytes as usize])
            }

            fn release(&mut self) {}
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source");
        let mut lease = SourceLease::open(Box::new(Endless)).unwrap();
        assert!(lease.copy_to(&mut File::create(path).unwrap()).is_err());
    }
}
