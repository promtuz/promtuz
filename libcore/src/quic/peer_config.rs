use std::sync::Arc;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::VerifyingKey;
use quinn::ClientConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureAlgorithm;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::AlgorithmIdentifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::pki_types::UnixTime;
use rustls::server::danger::ClientCertVerified;
use rustls::server::danger::ClientCertVerifier;
use rustls::sign::CertifiedKey;

use crate::data::identity::IdentitySigner;
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

/// Custom rustls SigningKey that wraps IdentitySigner for on-demand signing.
/// The secret key is only decrypted during the actual sign operation.
#[derive(Debug)]
struct IdentitySigningKey {
    public_key: VerifyingKey,
    signer: Arc<IdentitySigner>,
}

impl rustls::sign::SigningKey for IdentitySigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&SignatureScheme::ED25519) {
            Some(Box::new(IdentityTlsSigner {
                signer: self.signer.clone(),
            }))
        } else {
            None
        }
    }

    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        // Ed25519 AlgorithmIdentifier OID: 1.3.101.112
        let alg_id = AlgorithmIdentifier::from_slice(&[0x06, 0x03, 0x2b, 0x65, 0x70]);
        Some(rustls::sign::public_key_to_spki(&alg_id, self.public_key.as_bytes()))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ED25519
    }
}

/// Custom rustls Signer that performs on-demand signing via IdentitySigner.
#[derive(Debug)]
struct IdentityTlsSigner {
    signer: Arc<IdentitySigner>,
}

impl rustls::sign::Signer for IdentityTlsSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.signer
            .sign(message)
            .map(|sig| sig.to_bytes().to_vec())
            .map_err(|e| rustls::Error::General(e.to_string()))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// Generates an X.509 certificate using the peer's identity key.
/// The cert embeds the identity public key for authenticity.
fn generate_identity_cert(identity: &PeerIdentity) -> Result<CertifiedKey> {
    let public_key = identity.public_key;

    // Sign the TBS certificate using identity signer
    let tbs = build_tbs_certificate(public_key.as_bytes());
    let signature = identity.signer.sign(&tbs)?;

    let cert_der = build_certificate_der(&tbs, &signature.to_bytes());
    let certs = vec![CertificateDer::from(cert_der)];

    // Create custom signing key that signs on-demand
    let signing_key: Arc<dyn rustls::sign::SigningKey> = Arc::new(IdentitySigningKey {
        public_key,
        signer: identity.signer.clone(),
    });

    Ok(CertifiedKey::new(certs, signing_key))
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

/// Builds server config for P2P connections.
/// Uses identity-based certs with on-demand signing.
pub fn build_peer_server_cfg(identity: &PeerIdentity) -> Result<ServerConfig> {
    let certified_key = generate_identity_cert(identity)?;

    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(SkipClientVerifier))
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    crypto.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    Ok(ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?)))
}

/// Builds client config for P2P connections.
/// Uses identity-based certs with on-demand signing.
pub fn build_peer_client_cfg(identity: &PeerIdentity) -> Result<ClientConfig> {
    let certified_key = generate_identity_cert(identity)?;

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerifier))
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}

/// Extracts the Ed25519 identity public key from a peer's certificate.
///
/// The certificate structure we generate has the public key at a known location
/// within the SubjectPublicKeyInfo field of the TBSCertificate.
pub fn extract_peer_public_key(conn: &quinn::Connection) -> Option<[u8; 32]> {
    // Get peer certificates from the connection
    let peer_identity = conn.peer_identity()?;
    let certs = peer_identity.downcast_ref::<Vec<CertificateDer<'static>>>()?;
    let cert = certs.first()?;

    extract_ed25519_pubkey_from_cert(cert.as_ref())
}

/// Parse Ed25519 public key from certificate DER bytes.
///
/// Our minimal certificate structure:
/// Certificate ::= SEQUENCE {
///   TBSCertificate ::= SEQUENCE {
///     version [0] INTEGER,
///     serialNumber INTEGER,
///     signature AlgorithmIdentifier,
///     issuer Name (empty),
///     validity Validity,
///     subject Name (empty),
///     subjectPublicKeyInfo ::= SEQUENCE {
///       algorithm AlgorithmIdentifier (Ed25519 OID),
///       subjectPublicKey BIT STRING (33 bytes: 0x00 + 32-byte key)
///     }
///   },
///   signatureAlgorithm AlgorithmIdentifier,
///   signature BIT STRING
/// }
fn extract_ed25519_pubkey_from_cert(cert_der: &[u8]) -> Option<[u8; 32]> {
    // Look for the Ed25519 SPKI pattern:
    // SEQUENCE (0x30, 0x2a = 42 bytes) containing:
    //   SEQUENCE (0x30, 0x05) with Ed25519 OID (0x06, 0x03, 0x2b, 0x65, 0x70)
    //   BIT STRING (0x03, 0x21, 0x00) followed by 32-byte public key

    let spki_pattern: &[u8] = &[
        0x30, 0x2a,             // SEQUENCE, 42 bytes
        0x30, 0x05,             // SEQUENCE, 5 bytes (AlgorithmIdentifier)
        0x06, 0x03, 0x2b, 0x65, 0x70,  // OID 1.3.101.112 (Ed25519)
        0x03, 0x21, 0x00,       // BIT STRING, 33 bytes, 0 unused bits
    ];

    // Find the SPKI pattern in the certificate
    let pos = cert_der.windows(spki_pattern.len())
        .position(|window| window == spki_pattern)?;

    // Public key starts right after the pattern
    let key_start = pos + spki_pattern.len();
    let key_end = key_start + 32;

    if key_end > cert_der.len() {
        return None;
    }

    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&cert_der[key_start..key_end]);
    Some(pubkey)
}
