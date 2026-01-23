use anyhow::Result;
use common::quic::id::UserId;
use ed25519_dalek::SigningKey;
use ed25519_dalek::VerifyingKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use jni::JNIEnv;
use rcgen::Certificate;
use rustls::pki_types::PrivatePkcs8KeyDer;

use crate::api::CERTIFICATE;
use crate::data::identity::Identity;

pub struct PeerIdentity {
    pub certificate: Certificate,
    pub public_key: VerifyingKey,
}

impl PeerIdentity {
    pub fn initialize(env: &mut JNIEnv) -> Result<Self> {
        let isk = { SigningKey::from_bytes(&*Identity::secret_key(env)?) };

        let public_key = isk.verifying_key();
        let cert = Self::generate_cert(&isk)?;

        CERTIFICATE.set(cert.clone()).unwrap_or_else(|_| {
            log::error!("ERROR: failed to set global client certificate");
        });

        Ok(Self { certificate: cert, public_key })
    }

    fn generate_cert(isk: &SigningKey) -> Result<Certificate> {
        let der = isk.to_pkcs8_der()?;
        let isk_der = PrivatePkcs8KeyDer::from(der.as_bytes());
        let key_pair =
            rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&isk_der, &rcgen::PKCS_ED25519)?;

        let user_id = UserId::derive(isk.verifying_key().as_bytes()).to_string();
        let mut params = rcgen::CertificateParams::new(vec![user_id.clone()])?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params.distinguished_name.push(rcgen::DnType::CommonName, &user_id);

        params.key_usages =
            vec![rcgen::KeyUsagePurpose::DigitalSignature, rcgen::KeyUsagePurpose::KeyAgreement];

        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];

        Ok(params.self_signed(&key_pair)?)
    }
}
