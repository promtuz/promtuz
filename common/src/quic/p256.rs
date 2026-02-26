use std::fs;
use std::path::Path;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::DecodePrivateKey;

use crate::error;

/// Tries to read a valid SEC1 PEM Private key
#[allow(clippy::result_unit_err)]
pub fn secret_from_key(key_path: &Path) -> Result<SigningKey, ()> {
    let pem = fs::read_to_string(key_path).map_err(|err| {
        error!("failed to read file {path:?}: {err}", path = &key_path);
    })?;

    let secret = SigningKey::from_pkcs8_pem(&pem).map_err(|err| {
        error!("failed to parse pkcs8 secret key: {err}");
    })?;

    Ok(secret)
}
