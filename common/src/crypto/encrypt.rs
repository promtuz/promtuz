use chacha20poly1305::AeadCore;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::Key;
use chacha20poly1305::KeyInit;
use chacha20poly1305::Nonce;
use chacha20poly1305::aead::AeadMutInPlace;
use chacha20poly1305::aead::OsRng;
use serde::Deserialize;
use serde::Serialize;

type Result<T> = std::result::Result<T, chacha20poly1305::Error>;

#[derive(Serialize, Deserialize, Debug)]
pub struct Encrypted {
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub cipher: Vec<u8>,
}

impl Encrypted {
    /// does't use nonce
    pub fn encrypt_once(data: &[u8], key: &[u8; 32], ad: &[u8]) -> Vec<u8> {
        let mut chacha20 = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce = Nonce::from_slice(&[0u8; 12]);
        let mut cipher = Vec::from(data);

        if chacha20.encrypt_in_place(nonce, ad, &mut cipher).is_err() {
            cipher.clear();
        }

        cipher
    }

    pub fn encrypt(data: &[u8], key: &[u8; 32], ad: &[u8]) -> Encrypted {
        let mut chacha20 = ChaCha20Poly1305::new(Key::from_slice(key));
        let nonce = ChaCha20Poly1305::generate_nonce(OsRng);
        let mut cipher = Vec::from(data);

        if chacha20.encrypt_in_place(&nonce, ad, &mut cipher).is_err() {
            // returning empty cipher would be safer then returning plaintext
            // in case of any potential error that is
            cipher.clear();
        }

        Encrypted { nonce: nonce.to_vec(), cipher }
    }

    pub fn decrypt(self, key: &[u8; 32], ad: &[u8]) -> Result<Vec<u8>> {
        let mut chacha20 = ChaCha20Poly1305::new(Key::from_slice(key));
        let mut buffer = self.cipher;
        chacha20.decrypt_in_place(self.nonce.as_slice().into(), ad, &mut buffer).map(|_| buffer)
    }

    /// Encode an Encrypted (nonce + cipher) into a flat byte vec.
    /// Layout: [12-byte nonce][ciphertext...]
    pub fn flat(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.nonce.len() + self.cipher.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.cipher);
        out
    }
}
