use std::{fs, path::Path};

use anyhow::Result;
use p256::{SecretKey, pkcs8::DecodePrivateKey};


/// Tries to read a valid SEC1 PEM Private key at `key_path`
pub fn secret_from_key(key_path: &Path) -> Result<SecretKey> {
    let sec = fs::read_to_string(key_path)?;

    if sec.starts_with("-----BEGIN EC PRIVATE KEY-----") {
        Ok(SecretKey::from_sec1_pem(&sec)?)
    } else {
        Ok(SecretKey::from_pkcs8_pem(&sec)?)
    }
}
