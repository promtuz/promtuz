use std::sync::Arc;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use quinn::ClientConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureScheme;
use rustls::client::ResolvesClientCert;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;
use rustls::server::ResolvesServerCert;
use rustls::server::danger::ClientCertVerified;
use rustls::server::danger::ClientCertVerifier;
use rustls::sign::CertifiedKey;
use rustls::sign::SigningKey;

use crate::quic::keystore_signer::KeystoreSigner;
use crate::quic::peer_identity::PeerIdentity;

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

#[derive(Debug)]
struct CustomCertResolver {
    certified_key: Arc<CertifiedKey>,
}

impl CustomCertResolver {
    fn new(cert_chain: Vec<CertificateDer<'static>>, signer: Arc<dyn SigningKey>) -> Self {
        let certified_key = Arc::new(CertifiedKey::new(cert_chain, signer));
        Self { certified_key }
    }
}

impl ResolvesServerCert for CustomCertResolver {
    fn resolve(&self, _client_hello: rustls::server::ClientHello) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.certified_key))
    }
}

impl ResolvesClientCert for CustomCertResolver {
    fn resolve(
        &self, _root_hint_subjects: &[&[u8]], _sigschemes: &[SignatureScheme],
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(Arc::clone(&self.certified_key))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Builds server config where clients connect with each other
/// basically peer to peer
pub fn build_peer_server_cfg(identity: &PeerIdentity) -> Result<ServerConfig> {
    let cert_der = CertificateDer::from(identity.certificate.der().to_vec());

    // Create signer that fetches key on-demand from Android Keystore
    let signer: Arc<dyn SigningKey> =
        Arc::new(KeystoreSigner::new(identity.public_key.to_bytes().to_vec()));

    // Create cert resolver with certificate and lazy signer
    let cert_resolver = Arc::new(CustomCertResolver::new(vec![cert_der.clone()], signer));

    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(PeerClientVerifier))
        .with_cert_resolver(cert_resolver);

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

pub fn build_peer_client_cfg(identity: &PeerIdentity) -> Result<ClientConfig> {
    // Get the certificate (no private key involved)
    let cert_der = CertificateDer::from(identity.certificate.der().to_vec());

    // Create signer that fetches key on-demand from Android Keystore
    let signer: Arc<dyn SigningKey> =
        Arc::new(KeystoreSigner::new(identity.public_key.to_bytes().to_vec()));

    // Create cert resolver for client authentication
    let cert_resolver = Arc::new(CustomCertResolver::new(vec![cert_der.clone()], signer));

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerServerVerifier))
        .with_client_cert_resolver(cert_resolver);

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));

    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}
