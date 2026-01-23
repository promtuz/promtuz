use std::sync::Arc;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
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

use crate::quic::peer_identity::PeerIdentity;

/// Verifier that accepts any client cert (identity verified post-handshake)
#[derive(Debug)]
struct SkipClientVerifier;

impl ClientCertVerifier for SkipClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Accept any cert - real identity verification happens post-handshake
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

/// Verifier that accepts any server cert (identity verified post-handshake)
#[derive(Debug)]
struct SkipServerVerifier;

impl ServerCertVerifier for SkipServerVerifier {
    fn verify_server_cert(
        &self, _end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Accept any cert - real identity verification happens post-handshake
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

/// Generates a minimal self-signed X.509 certificate for TLS transport.
/// This cert has no identity meaning - it's purely for TLS encryption.
/// Real peer identity is verified post-handshake via ed25519 challenge-response.
fn generate_ephemeral_cert() -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    // Generate random key bytes using rand crate
    let key_bytes: [u8; 32] = rand::random();
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let public_key = signing_key.verifying_key();

    // Build minimal self-signed X.509 v3 certificate
    // Structure: TBSCertificate + SignatureAlgorithm + Signature
    let tbs = build_tbs_certificate(public_key.as_bytes());
    let signature = signing_key.sign(&tbs);

    let cert_der = build_certificate_der(&tbs, &signature.to_bytes());

    // PKCS#8 wrap the ed25519 private key
    let pkcs8_der = build_pkcs8_ed25519(&signing_key);

    Ok((
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pkcs8_der)),
    ))
}

/// Build TBSCertificate (the part that gets signed)
fn build_tbs_certificate(public_key: &[u8; 32]) -> Vec<u8> {
    // OID for Ed25519: 1.3.101.112
    let ed25519_oid: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

    // SubjectPublicKeyInfo for Ed25519
    let spki = [
        &[0x30, 0x2a][..], // SEQUENCE, 42 bytes
        &[0x30, 0x05][..], // SEQUENCE (AlgorithmIdentifier), 5 bytes
        ed25519_oid,
        &[0x03, 0x21, 0x00][..], // BIT STRING, 33 bytes, 0 unused bits
        public_key,
    ]
    .concat();

    // Serial number (random-ish, using first 8 bytes of pubkey)
    let serial = &public_key[0..8];

    // Validity: not before = 0 (1970), not after = 2050
    let validity: &[u8] = &[
        0x30, 0x1e, // SEQUENCE, 30 bytes
        0x17, 0x0d, // UTCTime, 13 bytes
        b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z',
        0x17, 0x0d, // UTCTime, 13 bytes
        b'5', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z',
    ];

    // Empty issuer and subject (minimal cert)
    let empty_name: &[u8] = &[0x30, 0x00]; // SEQUENCE, 0 bytes

    // Version 3 (explicit tag [0])
    let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];

    // Serial number (INTEGER)
    let serial_der = [&[0x02, serial.len() as u8][..], serial].concat();

    // Signature algorithm (Ed25519)
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    // Assemble TBSCertificate
    let tbs_content = [
        version,
        &serial_der,
        sig_alg,
        empty_name,  // issuer
        validity,
        empty_name,  // subject
        &spki,
    ]
    .concat();

    // Wrap in SEQUENCE
    encode_sequence(&tbs_content)
}

/// Build the final certificate DER
fn build_certificate_der(tbs: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    // Signature algorithm (Ed25519)
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    // Signature as BIT STRING
    let sig_bitstring = [&[0x03, 0x41, 0x00][..], signature].concat();

    // Assemble Certificate
    let cert_content = [tbs, sig_alg, &sig_bitstring].concat();

    encode_sequence(&cert_content)
}

/// Encode data as DER SEQUENCE with proper length encoding
fn encode_sequence(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    if len < 128 {
        [&[0x30, len as u8][..], data].concat()
    } else if len < 256 {
        [&[0x30, 0x81, len as u8][..], data].concat()
    } else {
        let len_bytes = (len as u16).to_be_bytes();
        [&[0x30, 0x82][..], &len_bytes, data].concat()
    }
}

/// Build PKCS#8 wrapper for Ed25519 private key
fn build_pkcs8_ed25519(signing_key: &SigningKey) -> Vec<u8> {
    // PKCS#8 structure for Ed25519:
    // SEQUENCE {
    //   INTEGER 0 (version)
    //   SEQUENCE { OID 1.3.101.112 } (algorithm)
    //   OCTET STRING { OCTET STRING { private key } }
    // }
    let version: &[u8] = &[0x02, 0x01, 0x00];
    let algorithm: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    // Private key wrapped in OCTET STRING inside OCTET STRING
    let key_bytes = signing_key.to_bytes();
    let inner_octet = [&[0x04, 0x20][..], &key_bytes].concat();
    let outer_octet = [&[0x04, (inner_octet.len()) as u8][..], &inner_octet].concat();

    let content = [version, algorithm, &outer_octet].concat();
    encode_sequence(&content)
}

/// Builds server config for P2P connections.
/// Uses ephemeral certs - identity verification happens post-handshake.
pub fn build_peer_server_cfg(_identity: &PeerIdentity) -> Result<ServerConfig> {
    let (certs, key) = generate_ephemeral_cert()?;

    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(SkipClientVerifier))
        .with_single_cert(certs, key)?;

    crypto.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    Ok(ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?)))
}

/// Builds client config for P2P connections.
/// Uses ephemeral certs - identity verification happens post-handshake.
pub fn build_peer_client_cfg(_identity: &PeerIdentity) -> Result<ClientConfig> {
    let (certs, key) = generate_ephemeral_cert()?;

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerifier))
        .with_client_auth_cert(certs, key)?;

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}
