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

/// Loads an Ed25519 PKCS#8 PEM key from disk, generating one on first run.
///
/// On first boot the operator typically does not have a separate identity
/// key on disk yet (that's what `secret_from_key` is for in the TLS path,
/// which is shipped with `certgen`). Rather than force them to run another
/// tool just for this, we generate a fresh Ed25519 key, persist it as PKCS#8
/// PEM with `0o600` permissions, and continue.
///
/// The resulting file holds the relay's *long-term identity* secret —
/// distinct from the TLS server key — and is what `RelayHello` to the
/// resolver is signed with. Treat the file like an SSH host key.
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

    if let Some(parent) = key_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).map_err(|err| {
                error!("failed to create parent dir {p:?}: {err}", p = parent);
            })?;
        }
    }

    // ed25519-dalek 2.x's `SigningKey::generate` takes a `rand_core` 0.6
    // CSPRNG, but the workspace's `rand` crate sits on rand_core 0.9.
    // Avoid the version mismatch by sampling 32 bytes directly from the
    // OS and feeding them into `SigningKey::from_bytes` (which is the
    // documented constructor for Ed25519 seed material).
    use rand::TryRngCore;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng
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
    // Non-unix targets: fall back to the basic loader. On Windows the
    // permission story is different and out of scope for now — we expect
    // operators on Windows to provision the key themselves.
    secret_from_key(key_path)
}
