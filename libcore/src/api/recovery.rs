//! Recovery exports — the ONLY places raw isk material crosses the FFI
//! (IDENTITY_RECOVERY.md §6). The platform MUST device-auth-gate
//! [`export_recovery_phrase`] and [`escrow_secret`] (biometric / device
//! credential) — libcore cannot enforce that from below the boundary.

use crate::data::recovery;
use crate::platform::CoreError;

/// The identity as a 24-word BIP39 phrase (Channel B). **Auth-gate on the
/// platform side is mandatory** — this is the private key, in words.
#[uniffi::export]
pub fn export_recovery_phrase() -> Result<Vec<String>, CoreError> {
    Ok(recovery::phrase()?)
}

/// Restore identity from a typed phrase. `name` is the user-prompted display
/// name (the phrase encodes only the secret); a later `backup_import`
/// overwrites it with the backed-up one. Fails if an identity already exists
/// or the checksum rejects the words.
#[uniffi::export]
pub fn restore_from_phrase(words: Vec<String>, name: String) -> Result<(), CoreError> {
    Ok(recovery::restore_from_phrase(&words, &name)?)
}

/// The raw isk for platform escrow (Channel A: Block Store / iCloud
/// Keychain). **Auth-gate on the platform side is mandatory.**
#[uniffi::export]
pub fn escrow_secret() -> Result<Vec<u8>, CoreError> {
    Ok(recovery::escrow_isk()?)
}

/// Restore identity from escrowed bytes (Channel A hit on fresh install).
/// `name` may be a placeholder — `backup_import` replaces it.
#[uniffi::export]
pub fn adopt_escrowed_secret(isk: Vec<u8>, name: String) -> Result<(), CoreError> {
    Ok(recovery::adopt_escrowed(&isk, &name)?)
}

/// Snapshot history + contacts + name into one encrypted blob. The platform
/// owns cadence (daily, dirty-flag off `on_db_changed`) and placement (Drive
/// app-folder / iCloud). Ciphertext-only to the cloud; the key derives from
/// the isk, so no separate backup password exists.
#[uniffi::export]
pub fn backup_export() -> Result<Vec<u8>, CoreError> {
    Ok(crate::data::backup::export()?)
}

/// Restore a backup blob into the local DBs (after identity restore — the
/// key derives from the isk). Idempotent; also restores the display name.
#[uniffi::export]
pub fn backup_import(blob: Vec<u8>) -> Result<(), CoreError> {
    Ok(crate::data::backup::import(&blob)?)
}
