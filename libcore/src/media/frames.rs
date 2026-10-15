use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgba8,
    Rgba16,
}

impl PixelFormat {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgba8 => 4,
            Self::Rgba16 => 8,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Rgba8 => 1,
            Self::Rgba16 => 2,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(Self::Rgba8),
            2 => Ok(Self::Rgba16),
            _ => bail!("invalid frame format"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub duration_ms: u32,
}

impl Frame {
    pub fn new(
        pixels: Vec<u8>, width: u32, height: u32, format: PixelFormat, duration_ms: u32,
    ) -> Result<Self> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height).ok().and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(format.bytes_per_pixel()));
        ensure!(width > 0 && height > 0 && expected == Some(pixels.len()), "invalid frame buffer");
        Ok(Self { pixels, width, height, format, duration_ms })
    }

    fn encoded_len(&self) -> usize {
        4 + 4 + 1 + 4 + 8 + self.pixels.len()
    }
}

#[derive(Clone, Copy)]
struct StoredFrame {
    offset: u64,
    bytes: u64,
}

enum Storage {
    Memory(Vec<Frame>),
    Spool { file: tempfile::NamedTempFile, frames: Vec<StoredFrame> },
}

pub struct FrameStore {
    storage: Storage,
    memory_limit: u64,
    byte_limit: u64,
    bytes: u64,
}

impl FrameStore {
    pub fn new(memory_limit: u64, byte_limit: u64) -> Self {
        Self { storage: Storage::Memory(Vec::new()), memory_limit, byte_limit, bytes: 0 }
    }

    pub fn push(&mut self, frame: Frame) -> Result<()> {
        let frame_bytes = u64::try_from(frame.encoded_len())
            .map_err(|_| anyhow::anyhow!("frame is too large"))?;
        let next = self
            .bytes
            .checked_add(frame_bytes)
            .ok_or_else(|| anyhow::anyhow!("frame store overflow"))?;
        ensure!(next <= self.byte_limit, "media processing exceeds its private storage limit");
        match &mut self.storage {
            Storage::Memory(frames) if next <= self.memory_limit => frames.push(frame),
            Storage::Memory(_) => self.spool(frame)?,
            Storage::Spool { file, frames } => {
                let offset = file.as_file_mut().stream_position()?;
                write_frame(file.as_file_mut(), &frame)?;
                frames.push(StoredFrame { offset, bytes: frame_bytes });
            },
        }
        self.bytes = next;
        Ok(())
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            Storage::Memory(frames) => frames.len(),
            Storage::Spool { frames, .. } => frames.len(),
        }
    }

    pub fn frame(&mut self, index: usize) -> Result<Frame> {
        match &mut self.storage {
            Storage::Memory(frames) => frames
                .get(index)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("frame index out of range")),
            Storage::Spool { file, frames } => {
                let stored =
                    frames.get(index).ok_or_else(|| anyhow::anyhow!("frame index out of range"))?;
                file.as_file_mut().seek(SeekFrom::Start(stored.offset))?;
                let frame = read_frame(file.as_file_mut())?;
                ensure!(
                    u64::try_from(frame.encoded_len()).ok() == Some(stored.bytes),
                    "corrupt frame spool"
                );
                Ok(frame)
            },
        }
    }

    fn spool(&mut self, next: Frame) -> Result<()> {
        let Storage::Memory(frames) =
            std::mem::replace(&mut self.storage, Storage::Memory(Vec::new()))
        else {
            unreachable!();
        };
        let mut file = tempfile::NamedTempFile::new()?;
        let mut stored = Vec::with_capacity(frames.len() + 1);
        for frame in frames.into_iter().chain(std::iter::once(next)) {
            let offset = file.as_file_mut().stream_position()?;
            let bytes = u64::try_from(frame.encoded_len())
                .map_err(|_| anyhow::anyhow!("frame is too large"))?;
            write_frame(file.as_file_mut(), &frame)?;
            stored.push(StoredFrame { offset, bytes });
        }
        self.storage = Storage::Spool { file, frames: stored };
        Ok(())
    }
}

fn write_frame(writer: &mut std::fs::File, frame: &Frame) -> Result<()> {
    writer.write_all(&frame.width.to_le_bytes())?;
    writer.write_all(&frame.height.to_le_bytes())?;
    writer.write_all(&[frame.format.tag()])?;
    writer.write_all(&frame.duration_ms.to_le_bytes())?;
    writer.write_all(&(u64::try_from(frame.pixels.len())?).to_le_bytes())?;
    writer.write_all(&frame.pixels)?;
    Ok(())
}

fn read_frame(reader: &mut std::fs::File) -> Result<Frame> {
    let mut width = [0; 4];
    let mut height = [0; 4];
    let mut format = [0; 1];
    let mut duration = [0; 4];
    let mut length = [0; 8];
    reader.read_exact(&mut width)?;
    reader.read_exact(&mut height)?;
    reader.read_exact(&mut format)?;
    reader.read_exact(&mut duration)?;
    reader.read_exact(&mut length)?;
    let length = usize::try_from(u64::from_le_bytes(length))
        .map_err(|_| anyhow::anyhow!("frame is too large"))?;
    let mut pixels = vec![0; length];
    reader.read_exact(&mut pixels)?;
    Frame::new(
        pixels,
        u32::from_le_bytes(width),
        u32::from_le_bytes(height),
        PixelFormat::from_tag(format[0])?,
        u32::from_le_bytes(duration),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8) -> Frame {
        Frame::new(vec![value; 16], 2, 2, PixelFormat::Rgba8, 50).unwrap()
    }

    #[test]
    fn spills_complete_frames_without_changing_them() {
        let mut store = FrameStore::new(40, 256);
        store.push(frame(1)).unwrap();
        store.push(frame(2)).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.frame(0).unwrap(), frame(1));
        assert_eq!(store.frame(1).unwrap(), frame(2));
    }

    #[test]
    fn refuses_a_job_before_its_private_budget_is_exceeded() {
        let mut store = FrameStore::new(0, 37);
        store.push(frame(1)).unwrap();
        assert!(store.push(frame(2)).is_err());
        assert_eq!(store.len(), 1);
        assert_eq!(store.frame(0).unwrap(), frame(1));
    }
}
