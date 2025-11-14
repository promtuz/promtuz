use std::{fs, path::Path};

use anyhow::Result;
use p256::SecretKey;


/// Tries to read a valid SEC1 PEM Private key at `key_path`
pub fn secret_from_key(key_path: &Path) -> Result<SecretKey> {
    let sec = fs::read_to_string(key_path)?;
    Ok(SecretKey::from_sec1_pem(&sec)?)
}
