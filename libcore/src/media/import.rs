use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use super::ports::SourceLease;

/// A private, bounded copy of a platform-owned source. It is deleted unless a
/// later media job explicitly takes it over.
pub struct ImportedSource {
    path: PathBuf,
    size: u64,
}

impl ImportedSource {
    pub fn from_lease(mut source: SourceLease, directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory).context("create media import directory")?;

        for _ in 0..4 {
            let path = directory.join(format!("{}.source", Uuid::now_v7()));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create private media import"),
            };
            let result = source.copy_to(&mut file);
            drop(file);
            match result {
                Ok(size) => return Ok(Self { path, size }),
                Err(error) => {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                },
            }
        }
        anyhow::bail!("could not reserve private media import")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl Drop for ImportedSource {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::*;
    use crate::media::ports::MediaSource;

    struct Source {
        bytes: Vec<u8>,
        releases: Arc<Mutex<u32>>,
    }

    impl MediaSource for Source {
        fn length(&self) -> Result<Option<u64>> {
            Ok(None)
        }

        fn read(&mut self, offset: u64, max_bytes: u32) -> Result<Vec<u8>> {
            let start = usize::try_from(offset)?;
            let end = (start + max_bytes as usize).min(self.bytes.len());
            Ok(self.bytes[start..end].to_vec())
        }

        fn release(&mut self) {
            *self.releases.lock().unwrap() += 1;
        }
    }

    #[test]
    fn import_is_private_bounded_copy_and_releases_source() {
        let directory = tempfile::tempdir().unwrap();
        let releases = Arc::new(Mutex::new(0));
        let lease = SourceLease::open(Box::new(Source {
            bytes: vec![4; 12], releases: releases.clone(),
        })).unwrap();
        let imported = ImportedSource::from_lease(lease, directory.path()).unwrap();
        assert_eq!(imported.size(), 12);
        assert_eq!(std::fs::read(imported.path()).unwrap(), vec![4; 12]);
        assert_eq!(*releases.lock().unwrap(), 1);
        let path = imported.path().to_owned();
        drop(imported);
        assert!(!path.exists());
    }
}
