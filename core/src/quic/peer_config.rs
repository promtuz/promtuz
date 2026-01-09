use std::sync::Arc;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use quinn::ClientConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
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

#[derive(Debug)]
struct PeerClientVerifier;

impl ClientCertVerifier for PeerClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
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

    // rcgen::PKCS_ED25519
    let params = rcgen::CertificateParams::new(vec![])?;

    let cert = params.self_signed(&key_pair)?;
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
        &self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>, ocsp_response: &[u8], now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // todo!()

        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

pub fn build_peer_client_cfg() -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerServerVerifier))
        .with_no_client_auth();

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));

    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}
