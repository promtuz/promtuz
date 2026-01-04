use common::crypto::get_signing_key;
use jni::JNIEnv;
use jni::objects::JString;
use jni_macro::jni;
use unicode_normalization::UnicodeNormalization;

use crate::JC;
use crate::data::identity::Identity;
use crate::db::identity::IdentityRow;
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

    // Generating long-term identity secret key
    let isk = get_signing_key();

    let ipk = isk.verifying_key();

    // ENCRYPTING THE KEY
    let enc_isk = key_manager.encrypt(isk.as_bytes()).unwrap();

    let identity = IdentityRow {
        id: 0,
        ipk: ipk.to_bytes(),
        enc_isk,
        name,
        created_at: systime().as_millis() as u64,
    };

    Identity::save(identity).is_ok()
}
