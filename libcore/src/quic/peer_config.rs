use std::sync::Arc;

use anyhow::Result;
use common::quic::id::UserId;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use quinn::ClientConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rcgen::KeyPair;
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use rustls::server::danger::ClientCertVerified;
use rustls::server::danger::ClientCertVerifier;

use crate::api::CERTIFICATE;

#[derive(Debug)]
struct PeerClientVerifier;

impl ClientCertVerifier for PeerClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // let (_, cert) = X509Certificate::from_der(end_entity.as_ref()).map_err(|_| {
        //     rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
        // })?;
        // todo!()

        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _msg: &[u8], _crt: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, _msg: &[u8], _crt: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Builds server config where clients connect with each other
/// basically peer to peer
pub fn build_peer_server_cfg(isk: SigningKey) -> Result<ServerConfig> {
    let der = isk.to_pkcs8_der()?;
    let isk_der = PrivatePkcs8KeyDer::from(der.as_bytes());

    let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(&isk_der, &rcgen::PKCS_ED25519)?;

    let user_id = UserId::derive(isk.verifying_key().as_bytes()).to_string();

    // rcgen::PKCS_ED25519
    let mut params = rcgen::CertificateParams::new(vec![user_id.clone()])?;

    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(rcgen::DnType::CommonName, &user_id);

    let cert = params.self_signed(&key_pair)?;

    CERTIFICATE.set(cert.clone()).unwrap_or_else(|_| {
        log::error!("ERROR: failed to set global client certificate");
    });

    let cert_der = CertificateDer::from(cert.der().to_vec());

    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(PeerClientVerifier))
        .with_single_cert(vec![cert_der], key_der)?;

    crypto.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    Ok(ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?)))
}

#[derive(Debug)]
struct PeerServerVerifier;

impl ServerCertVerifier for PeerServerVerifier {
    fn verify_server_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // todo!()

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub fn build_peer_client_cfg(key_pair: KeyPair) -> Result<ClientConfig> {
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let cert_der = {
        // unwrapping as `..._server_cfg` must run before `..._client_cfg` setting `CERTIFICATE`
        let cert = CERTIFICATE.get().unwrap();
        CertificateDer::from(cert.der().to_vec())
    };

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerServerVerifier))
        .with_client_auth_cert(vec![cert_der], key_der)?;

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));

    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}
