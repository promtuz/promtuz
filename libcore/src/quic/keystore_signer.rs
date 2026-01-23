use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey as Ed25519SigningKey;
use rustls::SignatureAlgorithm;
use rustls::SignatureScheme;
use rustls::sign::SigningKey;

use crate::JVM;
use crate::data::identity::Identity;

#[derive(Debug)]
pub struct KeystoreSigner {
    public_key: Vec<u8>,
    scheme: SignatureScheme,
}

impl KeystoreSigner {
    pub fn new(public_key: Vec<u8>) -> Self {
        Self { public_key, scheme: SignatureScheme::ED25519 }
    }

    fn fetch_and_sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let mut env = JVM
            .get()
            .ok_or_else(|| rustls::Error::General("JVM not initialized".into()))?
            .attach_current_thread()
            .map_err(|e| rustls::Error::General(format!("JNI attach failed: {}", e)))?;

        let isk_bytes = Identity::secret_key(&mut env)
            .map_err(|e| rustls::Error::General(format!("Failed to fetch key: {}", e)))?;

        let signing_key = Ed25519SigningKey::from_bytes(&isk_bytes);
        let signature = signing_key.sign(message);

        Ok(signature.to_bytes().to_vec())
    }
}

impl rustls::sign::Signer for KeystoreSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.fetch_and_sign(message)
    }

    fn scheme(&self) -> SignatureScheme {
        self.scheme
    }
}

impl SigningKey for KeystoreSigner {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&self.scheme) {
            Some(Box::new(Self { public_key: self.public_key.clone(), scheme: self.scheme }))
        } else {
            None
        }
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}
