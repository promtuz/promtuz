//! The only places raw isk material crosses the FFI. The platform must gate
//! [`export_recovery_phrase`] and [`escrow_secret`] behind device auth; core cannot enforce it.

use crate::data::backup::BackupMergeReport;
use crate::data::recovery;
use crate::platform::CoreError;

/// The private key as a 24-word BIP39 phrase.
#[uniffi::export]
pub fn export_recovery_phrase() -> Result<Vec<String>, CoreError> {
    Ok(recovery::phrase()?)
}

/// The phrase encodes only the secret, so `name` is asked for; a later `backup_import` replaces it.
#[uniffi::export]
pub fn restore_from_phrase(words: Vec<String>, name: String) -> Result<(), CoreError> {
    Ok(recovery::restore_from_phrase(&words, &name)?)
}

/// The raw isk for platform escrow (Block Store, iCloud Keychain).
#[uniffi::export]
pub fn escrow_secret() -> Result<Vec<u8>, CoreError> {
    Ok(recovery::escrow_isk()?)
}

/// `name` may be a placeholder; `backup_import` replaces it.
#[uniffi::export]
pub fn adopt_escrowed_secret(isk: Vec<u8>, name: String) -> Result<(), CoreError> {
    Ok(recovery::adopt_escrowed(&isk, &name)?)
}

/// History, contacts and name in one blob encrypted under a key derived from the isk. The platform
/// owns cadence and placement; only ciphertext reaches the cloud.
#[uniffi::export]
pub fn backup_export() -> Result<Vec<u8>, CoreError> {
    Ok(crate::data::backup::export()?)
}

/// Needs the identity restored first, since the key derives from the isk. Idempotent.
#[uniffi::export]
pub fn backup_import(blob: Vec<u8>) -> Result<(), CoreError> {
    crate::data::backup::import(&blob, true)?;
    Ok(())
}

/// Additive restore for a live DB: inserts only missing rows, never replaces, deletes or renames.
/// The blob is editable by whoever holds the isk, so a live row always wins a collision.
#[uniffi::export]
pub fn backup_import_merge(blob: Vec<u8>) -> Result<BackupMergeReport, CoreError> {
    Ok(crate::data::backup::import(&blob, false)?)
}
