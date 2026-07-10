//! Recovery channels (IDENTITY_RECOVERY.md): the isk rendered as a BIP39
//! phrase (Channel B) or handed raw to platform escrow (Channel A). Lives
//! inside `data/` so it can reach `Identity::secret_key_with_manager` — raw
//! key bytes still never leave the data layer except through the two
//! documented escrow/phrase exports.

use anyhow::Result;
use anyhow::anyhow;
use bip39::Mnemonic;
use zeroize::Zeroizing;

use crate::data::identity::Identity;

/// The current isk as a 24-word BIP39 phrase (32 bytes entropy + checksum).
/// Caller (FFI layer) documents the platform auth-gate requirement.
pub fn phrase() -> Result<Vec<String>> {
    let secret = Identity::secret_key_with_manager()?;
    let m = Mnemonic::from_entropy(&secret[..]).map_err(|e| anyhow!("mnemonic: {e}"))?;
    Ok(m.words().map(str::to_string).collect())
}

/// The raw isk for platform escrow (Block Store / iCloud Keychain).
pub fn escrow_isk() -> Result<Vec<u8>> {
    let secret = Identity::secret_key_with_manager()?;
    Ok(secret.to_vec())
}

/// Decode a 24-word phrase back to the isk. BIP39's checksum makes a typo an
/// error here rather than a silently different identity.
fn isk_from_phrase(words: &[String]) -> Result<Zeroizing<[u8; 32]>> {
    let normalized: Vec<String> = words.iter().map(|w| w.trim().to_lowercase()).collect();
    let joined = normalized.join(" ");
    // bip39's UnknownWord index is 0-based; humans count from 1. Re-map and
    // name the offender so the user isn't sent to the wrong word.
    let m = Mnemonic::parse_normalized(&joined).map_err(|e| match e {
        bip39::Error::UnknownWord(i) => anyhow!(
            "word {} (\"{}\") is not a recovery word",
            i + 1,
            normalized.get(i).map(String::as_str).unwrap_or("?")
        ),
        bip39::Error::InvalidChecksum => {
            anyhow!("a word is mistyped or out of order (checksum failed)")
        },
        other => anyhow!("invalid phrase: {other}"),
    })?;
    let entropy = Zeroizing::new(m.to_entropy());
    let isk: [u8; 32] =
        entropy.as_slice().try_into().map_err(|_| anyhow!("phrase must be 24 words"))?;
    Ok(Zeroizing::new(isk))
}

/// Channel B restore: phrase → isk → sealed identity.
pub fn restore_from_phrase(words: &[String], name: &str) -> Result<()> {
    let isk = isk_from_phrase(words)?;
    Identity::restore(&isk, name)
}

/// Channel A restore: escrowed bytes → sealed identity.
pub fn adopt_escrowed(isk: &[u8], name: &str) -> Result<()> {
    let isk: [u8; 32] =
        isk.try_into().map_err(|_| anyhow!("escrowed secret must be 32 bytes"))?;
    Identity::restore(&isk, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words_of(entropy: [u8; 32]) -> Vec<String> {
        Mnemonic::from_entropy(&entropy).unwrap().words().map(str::to_string).collect()
    }

    #[test]
    fn phrase_roundtrips_to_same_isk() {
        let entropy = [7u8; 32];
        let words = words_of(entropy);
        assert_eq!(words.len(), 24);
        let isk = isk_from_phrase(&words).unwrap();
        assert_eq!(&isk[..], &entropy[..]);
    }

    #[test]
    fn phrase_is_case_and_whitespace_tolerant() {
        let entropy = [42u8; 32];
        let words: Vec<String> =
            words_of(entropy).iter().map(|w| format!("  {}  ", w.to_uppercase())).collect();
        let isk = isk_from_phrase(&words).unwrap();
        assert_eq!(&isk[..], &entropy[..]);
    }

    #[test]
    fn wrong_word_fails_checksum() {
        let mut words = words_of([7u8; 32]);
        // Swap two distinct words — permutes the payload, breaks the checksum.
        assert_ne!(words[0], words[1], "fixture must have distinct words");
        words.swap(0, 1);
        assert!(isk_from_phrase(&words).is_err(), "tampered phrase must not decode");
    }

    #[test]
    fn unknown_word_error_is_one_based_and_names_the_word() {
        let mut words = words_of([7u8; 32]);
        words[16] = "guage".into(); // typo at human-word 17 (0-based 16)
        let err = isk_from_phrase(&words).unwrap_err().to_string();
        assert!(err.contains("word 17"), "must report the 1-based position: {err}");
        assert!(err.contains("guage"), "must name the offending word: {err}");
    }

    #[test]
    fn twelve_word_phrase_is_rejected() {
        // Valid 12-word mnemonic = 16 bytes entropy — wrong size for an isk.
        let words = Mnemonic::from_entropy(&[9u8; 16])
            .unwrap()
            .words()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert!(isk_from_phrase(&words).is_err());
    }
}
