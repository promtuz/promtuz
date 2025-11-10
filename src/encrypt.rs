use chacha20poly1305::{
    AeadCore, ChaCha20Poly1305, Key, KeyInit,
    aead::{AeadMutInPlace, OsRng},
};
use serde::{Deserialize, Serialize};

type ChachaRes<T> = Result<T, chacha20poly1305::Error>;

#[derive(Serialize, Deserialize, Debug)]
pub struct Encrypted {
    #[serde(with = "serde_bytes")]
    pub nonce: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub cipher: Vec<u8>,
}

#[allow(deprecated)] // https://github.com/RustCrypto/traits/issues/2036
impl Encrypted {
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

    pub fn decrypt(self, key: &[u8; 32], ad: &[u8]) -> ChachaRes<Vec<u8>> {
        let mut chacha20 = ChaCha20Poly1305::new(Key::from_slice(key));
        let mut buffer = self.cipher;
        chacha20
            .decrypt_in_place(self.nonce.as_slice().into(), ad, &mut buffer)
            .map(|_| buffer)
    }
}
