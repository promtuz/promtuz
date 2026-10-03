//! The only exports of the raw isk: a BIP39 phrase, or the bytes for platform escrow.

use anyhow::Result;
use anyhow::anyhow;
use bip39::Mnemonic;
use common::types::bytes::fixed;
use zeroize::Zeroizing;

use crate::data::identity::Identity;

pub fn phrase() -> Result<Vec<String>> {
    let secret = Identity::secret_key_with_manager()?;
    let m = Mnemonic::from_entropy(&secret[..]).map_err(|e| anyhow!("mnemonic: {e}"))?;
    Ok(m.words().map(str::to_string).collect())
}

pub fn escrow_isk() -> Result<Vec<u8>> {
    let secret = Identity::secret_key_with_manager()?;
    Ok(secret.to_vec())
}

fn isk_from_phrase(words: &[String]) -> Result<Zeroizing<[u8; 32]>> {
    let normalized: Vec<String> = words.iter().map(|w| w.trim().to_lowercase()).collect();
    let joined = normalized.join(" ");
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

pub fn restore_from_phrase(words: &[String], name: &str) -> Result<()> {
    let isk = isk_from_phrase(words)?;
    Identity::restore(&isk, name)
}

pub fn adopt_escrowed(isk: &[u8], name: &str) -> Result<()> {
    Identity::restore(&fixed::<32>(isk, "escrowed secret")?, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phrase as a user wrote it down. Every later build must restore the same key from it.
    const PHRASE: &str = "arctic live gadget display excess mandate sniff autumn people disorder \
        affair hole retreat fancy close tip deer village tuition orbit cannon owner maid spare";
    const ISK: &str = "0b30557a9fc4e90e33587da2c7ec11365b80a5caef14395e83a8cdf2173c6186";

    fn restore(words: &[String]) -> Result<String> {
        Ok(hex::encode(*isk_from_phrase(words)?))
    }

    #[test]
    fn a_written_down_phrase_keeps_restoring_the_same_key() {
        let words: Vec<String> = PHRASE.split_whitespace().map(String::from).collect();
        assert_eq!(restore(&words).unwrap(), ISK);
        let sloppy: Vec<String> = words.iter().map(|w| format!(" {} ", w.to_uppercase())).collect();
        assert_eq!(restore(&sloppy).unwrap(), ISK, "case and stray spaces do not matter");

        let mut swapped = words.clone();
        swapped.swap(3, 4);
        assert!(restore(&swapped).unwrap_err().to_string().contains("checksum"));
        let twelve = Mnemonic::from_entropy(&[1; 16]).unwrap();
        let twelve: Vec<String> = twelve.words().map(String::from).collect();
        assert!(restore(&twelve).unwrap_err().to_string().contains("24 words"));
    }
}
