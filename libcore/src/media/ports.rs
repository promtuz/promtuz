use anyhow::{Result, ensure};

use super::policy::MAX_SOURCE_BYTES;

pub const READ_CHUNK_BYTES: u32 = 1024 * 1024;

pub trait MediaSource: Send {
    fn length(&self) -> Result<u64>;
    fn read(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>>;
    fn release(&mut self);
}

pub struct SourceLease {
    source: Option<Box<dyn MediaSource>>,
    length: u64,
}

impl SourceLease {
    pub fn open(source: Box<dyn MediaSource>) -> Result<Self> {
        let length = source.length()?;
        ensure!(length > 0, "The selected file is empty.");
        ensure!(length <= MAX_SOURCE_BYTES, "The selected file exceeds the 100 MiB import limit.");
        Ok(Self { source: Some(source), length })
    }

    pub fn read_chunk(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
        ensure!(offset <= self.length, "source offset is out of range");
        ensure!(max_bytes <= READ_CHUNK_BYTES, "source read exceeds the 1 MiB adapter limit");
        let remaining = self.length - offset;
        let requested = u64::from(max_bytes).min(remaining);
        let bytes =
            self.source.as_deref_mut().expect("released source").read(offset, requested as u32)?;
        ensure!(
            u64::try_from(bytes.len())? <= requested,
            "source returned more bytes than requested"
        );
        Ok(bytes)
    }

    pub fn copy_to_memory(&mut self) -> Result<Vec<u8>> {
        let capacity =
            usize::try_from(self.length).map_err(|_| anyhow::anyhow!("source is too large"))?;
        let mut bytes = Vec::with_capacity(capacity);
        while bytes.len() < capacity {
            let offset = u64::try_from(bytes.len())?;
            let chunk = self.read_chunk(offset, READ_CHUNK_BYTES)?;
            ensure!(!chunk.is_empty(), "source ended before its reported length");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
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
        fn length(&self) -> Result<u64> {
            Ok(self.bytes.len() as u64)
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
        assert_eq!(lease.copy_to_memory().unwrap(), bytes);
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
}
