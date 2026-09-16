//! Filesystem and S3 storage with the same public object paths.

use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;

use crate::config::BackendConfig;
use crate::store::s3::S3;

pub enum Backend {
    Fs(PathBuf),
    S3(S3),
}

/// A sticker blob never changes under its name; a manifest does.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const MUTABLE: &str = "public, max-age=60";

impl Backend {
    pub fn from_config(cfg: &BackendConfig) -> Result<Self> {
        Ok(match cfg {
            BackendConfig::Fs { dir } => {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
                Self::Fs(dir.clone())
            },
            BackendConfig::S3 { endpoint, bucket, region, access_key, secret_key } => {
                Self::S3(S3::new(endpoint, bucket, region, access_key, secret_key)?)
            },
        })
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Fs(dir) => format!("fs:{}", dir.display()),
            Self::S3(_) => "s3".into(),
        }
    }

    pub async fn put(&self, key: &str, bytes: Vec<u8>, immutable: bool) -> Result<()> {
        match self {
            Self::Fs(dir) => {
                let path = dir.join(key);
                let parent = path.parent().context("object key has no directory")?;
                tokio::fs::create_dir_all(parent).await?;
                // Write beside, then rename: a reader on the served tree never
                // sees a half-written object.
                let tmp = tmp_path(&path);
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, &path).await?;
                Ok(())
            },
            Self::S3(s3) => s3.put(key, bytes, if immutable { IMMUTABLE } else { MUTABLE }).await,
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Fs(dir) => match tokio::fs::read(dir.join(key)).await {
                Ok(b) => Ok(Some(b)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Self::S3(s3) => s3.get(key).await,
        }
    }

    /// Gone afterwards, whether or not it was there.
    pub async fn delete(&self, key: &str) -> Result<()> {
        match self {
            Self::Fs(dir) => match tokio::fs::remove_file(dir.join(key)).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            },
            Self::S3(s3) => s3.delete(key).await,
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}
