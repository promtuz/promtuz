use std::{fs, path::Path};

use anyhow::Result;
use p256::pkcs8::DecodePrivateKey;

pub use p256::*;

use crate::error;

/// Tries to read a valid SEC1 PEM Private key at `
#[allow(clippy::result_unit_err)]
pub fn secret_from_key(key_path: &Path) -> Result<SecretKey, ()> {
    let sec = fs::read_to_string(key_path).map_err(|err| {
        error!("failed to read file {path:?}: {err}", path = &key_path);
    })?;

    let secret = if sec.starts_with("-----BEGIN EC PRIVATE KEY-----") {
        SecretKey::from_sec1_pem(&sec).map_err(|err| {
            error!("failed to parse sec1 secret key: {err}");
        })?
    } else {
        SecretKey::from_pkcs8_pem(&sec).map_err(|err| {
            error!("failed to parse pkcs8 secret key: {err}");
        })?
    };

    Ok(secret)
}
