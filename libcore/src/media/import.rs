use std::path::{Path, PathBuf};

use anyhow::Result;

use super::ports::SourceLease;

/// A private, bounded copy of a platform-owned source. It is deleted unless a
/// later media job explicitly takes it over.
pub struct ImportedSource {
    path: PathBuf,
    size: u64,
}

impl ImportedSource {
    pub fn from_lease(mut source: SourceLease, directory: &Path) -> Result<Self> {
        use std::io::{self,Read};
        let size=source.image_size()?;
        struct Reader<'a>{source:&'a mut SourceLease,offset:u64}
        impl Read for Reader<'_>{
            fn read(&mut self,output:&mut[u8])->io::Result<usize>{
                let bytes=self.source.read_chunk(self.offset,output.len().min(1024*1024) as u32).map_err(io::Error::other)?;
                output[..bytes.len()].copy_from_slice(&bytes);self.offset+=bytes.len() as u64;Ok(bytes.len())
            }
        }
        let (path,_)=crate::private_files::stage_reader_in(&mut Reader{source:&mut source,offset:0},size,directory)?;
        Ok(Self{path:path.into(),size})
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
        crate::private_files::remove_temporary(&self.path.to_string_lossy());
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
        let mut visible=Vec::new();
        std::io::Read::read_to_end(&mut crate::private_files::StorageFile::open(&imported.path().to_string_lossy(),false).unwrap(),&mut visible).unwrap();
        assert_eq!(visible,vec![4;12]);
        assert_ne!(std::fs::read(imported.path()).unwrap(),visible);
        assert_eq!(*releases.lock().unwrap(), 1);
        let path = imported.path().to_owned();
        drop(imported);
        assert!(!path.exists());
    }
}


/// Codec APIs require a seekable OS path. Android gets a RAM-backed anonymous file rather than
/// plaintext disk scratch. Encoded output remains capped by the media policy and is encrypted
/// immediately when staging takes ownership.
pub(crate) struct AnonymousFile {file:std::fs::File,path:PathBuf}
impl AnonymousFile {
    pub(crate) fn new()->Result<Self> {
        #[cfg(any(target_os="android",target_os="linux"))]
        {
            use std::os::fd::{FromRawFd,AsRawFd};
            let name=c"promtuz-media";
            // SAFETY: the constant is NUL terminated; ownership transfers only for a valid fd.
            let fd=unsafe{libc::memfd_create(name.as_ptr(),libc::MFD_CLOEXEC)};
            if fd<0{return Err(std::io::Error::last_os_error().into())}
            let file=unsafe{std::fs::File::from_raw_fd(fd)};
            let path=PathBuf::from(format!("/proc/self/fd/{}",file.as_raw_fd()));
            Ok(Self{file,path})
        }
        #[cfg(not(any(target_os="android",target_os="linux")))]
        {
            use std::os::fd::AsRawFd;
            let file=tempfile::tempfile()?;
            let path=PathBuf::from(format!("/dev/fd/{}",file.as_raw_fd()));
            Ok(Self{file,path})
        }
    }
    pub(crate) fn path(&self)->&Path {&self.path}
    pub(crate) fn size(&self)->Result<u64>{Ok(self.file.metadata()?.len())}
}
