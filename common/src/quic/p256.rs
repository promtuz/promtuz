use std::fs;
use std::path::Path;

use anyhow::Result;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::DecodePrivateKey;

use crate::error;

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

/// Loads the node's Ed25519 PKCS#8 key, creating it with mode 0600 on first run.
#[allow(clippy::result_unit_err)]
#[cfg(unix)]
pub fn secret_from_key_or_create(key_path: &Path) -> Result<SigningKey, ()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    if key_path.exists() {
        return secret_from_key(key_path);
    }

    crate::warn!(
        "identity key not found at {path:?}; generating a fresh Ed25519 keypair",
        path = key_path,
    );

    if let Some(parent) = key_path.parent()
        && !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|err| {
                error!("failed to create parent dir {p:?}: {err}", p = parent);
            })?;
        }

    use rand::TryRng;
    let mut seed = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut seed)
        .map_err(|err| error!("OS RNG failed: {err}"))?;
    let signing = SigningKey::from_bytes(&seed);

    let pem = signing
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|err| error!("failed to encode pkcs8 pem: {err}"))?;

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(key_path)
        .map_err(|err| error!("failed to create identity key {p:?}: {err}", p = key_path))?;
    file.write_all(pem.as_bytes())
        .map_err(|err| error!("failed to write identity key {p:?}: {err}", p = key_path))?;

    Ok(signing)
}

#[allow(clippy::result_unit_err)]
#[cfg(not(unix))]
pub fn secret_from_key_or_create(key_path: &Path) -> Result<SigningKey, ()> {
    // Non-unix operators provision the key themselves.
    secret_from_key(key_path)
}
