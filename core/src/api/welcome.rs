use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

use common::crypto::get_signing_key;
use common::crypto::get_static_keypair;
use jni::JNIEnv;
use jni::objects::JString;
use jni_macro::jni;
use log::info;
use unicode_normalization::UnicodeNormalization;

use crate::JC;
use crate::data::identity::Identity;
use crate::db::identity::IdentityRow;
use crate::jni_try;
use crate::ndk::key_manager::KeyManager;
use crate::unwrap_or_ret;
use crate::utils::systime;

fn validate_nickname(name: &str) -> Result<String, String> {
    // Normalize and trim
    let normalized: String = name.nfc().collect();
    let trimmed = normalized.trim();

    // Length check
    if trimmed.is_empty() {
        return Err("Nickname cannot be empty".into());
    }
    if trimmed.chars().count() > 32 {
        return Err("Nickname too long (max 32 characters)".into());
    }

    // Block control characters and zero-width
    if trimmed.chars().any(|c| c.is_control() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{FEFF}'))
    {
        return Err("Nickname contains invalid characters".into());
    }

    Ok(trimmed.to_string())
}

#[jni(base = "com.promtuz.core", class = "API")]
pub extern "system" fn welcome(mut env: JNIEnv, _: JC, name: JString) -> bool {
    let name: String = unsafe { env.get_string_unchecked(&name) }.unwrap().into();
    let name = unwrap_or_ret!(validate_nickname(&name), false);

    let key_manager = KeyManager::new(&mut env).unwrap();

    let (isk, ipk) = get_static_keypair();
    let vsk = get_signing_key();

    let vfk = vsk.verifying_key().to_bytes();

    // ENCRYPTING THE KEYS
    let enc_isk = key_manager.encrypt(isk.as_bytes()).unwrap();
    let enc_vsk = key_manager.encrypt(vsk.as_bytes()).unwrap();

    let identity = IdentityRow {
        id: 0,
        ipk: ipk.to_bytes(),
        vfk,
        enc_isk,
        enc_vsk,
        name,
        created_at: systime().as_millis() as u64,
    };

    let ok = catch_unwind(AssertUnwindSafe(|| Identity::save(identity).is_ok()))
        .map_err(|panic| {
            if let Some(s) = panic.downcast_ref::<&str>() {
                info!("PANIC: {}", s);
            } else if let Some(s) = panic.downcast_ref::<String>() {
                info!("PANIC: {}", s);
            } else {
                info!("PANIC: <non-string payload>");
            }
        })
        .is_ok();

    info!("Identity::save finished, ok={ok}");

    ok
}
