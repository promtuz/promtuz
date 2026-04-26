use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::Signature as Ed25519Signature;
use ed25519_dalek::Verifier;
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
use x509_parser::oid_registry::asn1_rs::oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

use crate::data::identity::IdentitySigner;
use crate::quic::peer_identity::PeerIdentity;

/// OID for Ed25519 signature/key algorithm (1.3.101.112 / id-Ed25519, RFC 8410).
const ED25519_OID: x509_parser::der_parser::Oid<'static> = oid!(1.3.101 .112);

// ===========================================================================
// Peer-to-peer TLS verifiers.
//
// These verifiers DO NOT validate:
//   * an issuer / CA chain (peer certs are self-signed; there is no PKI)
//   * notBefore/notAfter validity windows (peers don't share a clock and the
//     identity is the long-term Ed25519 SPKI, which doesn't expire)
//   * the certificate Subject / SAN (the peer's identity is its public key,
//     not a DNS name; we look it up out-of-band via the QR code)
//
// What they DO validate:
//   * the presented end-entity cert parses as X.509 with an Ed25519 SPKI
//   * (in `verify_tls13_signature`) the TLS handshake transcript signature
//     is a valid Ed25519 signature under the SPKI from the cert
//
// On top of that, the application performs `verify_peer_self_signature` on
// the peer cert post-handshake (see `extract_peer_public_key`). That step
// rejects certs whose embedded SPKI does not match the key that signed the
// cert itself, closing the substring-search spoof of the old extractor.
// ===========================================================================

/// Verifier for the P2P server side: validates the client's cert is a
/// well-formed Ed25519 X.509 cert and verifies the TLS handshake signature.
#[derive(Debug)]
struct PeerClientCertVerifier;

impl ClientCertVerifier for PeerClientCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Reject anything that isn't a parsable Ed25519 X.509 cert at handshake
        // time. Identity (matching the expected peer) is checked post-handshake
        // by `extract_peer_public_key`.
        ed25519_pubkey_from_cert_der(end_entity.as_ref())
            .ok_or_else(|| rustls::Error::General("peer cert is not a valid Ed25519 X.509".into()))?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _msg: &[u8], _crt: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, msg: &[u8], crt: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_ed25519(msg, crt, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Verifier for the P2P client side: validates the server's cert is a
/// well-formed Ed25519 X.509 cert and verifies the TLS handshake signature.
#[derive(Debug)]
struct PeerServerCertVerifier;

impl ServerCertVerifier for PeerServerCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        ed25519_pubkey_from_cert_der(end_entity.as_ref()).ok_or_else(|| {
            rustls::Error::General("peer cert is not a valid Ed25519 X.509".into())
        })?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_ed25519(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Shared implementation for verifying a TLS 1.3 handshake signature using the
/// Ed25519 SPKI extracted from the presented certificate.
fn verify_tls13_ed25519(
    message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    if dss.scheme != SignatureScheme::ED25519 {
        return Err(rustls::Error::General(format!(
            "unsupported handshake signature scheme: {:?}",
            dss.scheme
        )));
    }

    let pubkey_bytes = ed25519_pubkey_from_cert_der(cert.as_ref())
        .ok_or_else(|| rustls::Error::General("peer cert SPKI is not Ed25519".into()))?;
    let verifying_key = VerifyingKey::from_bytes(&pubkey_bytes)
        .map_err(|e| rustls::Error::General(format!("invalid Ed25519 SPKI: {e}")))?;

    let sig_bytes: &[u8] = dss.signature();
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| rustls::Error::General("Ed25519 handshake signature must be 64 bytes".into()))?;
    let signature = Ed25519Signature::from_bytes(&sig_arr);

    verifying_key
        .verify(message, &signature)
        .map_err(|e| rustls::Error::General(format!("Ed25519 handshake signature failed: {e}")))?;

    Ok(HandshakeSignatureValid::assertion())
}

/// Custom rustls SigningKey that wraps IdentitySigner for on-demand signing.
/// The secret key is only decrypted during the actual sign operation.
#[derive(Debug)]
struct IdentitySigningKey {
    public_key: VerifyingKey,
}

impl rustls::sign::SigningKey for IdentitySigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&SignatureScheme::ED25519) {
            Some(Box::new(IdentityTlsSigner))
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
struct IdentityTlsSigner;

impl rustls::sign::Signer for IdentityTlsSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        IdentitySigner::sign(message)
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
    let signature = IdentitySigner::sign(&tbs)?;

    let cert_der = build_certificate_der(&tbs, &signature.to_bytes());
    let certs = vec![CertificateDer::from(cert_der)];

    // Create custom signing key that signs on-demand
    let signing_key: Arc<dyn rustls::sign::SigningKey> =
        Arc::new(IdentitySigningKey { public_key });

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
        b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z', 0x17,
        0x0d, // UTCTime, 13 bytes
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
        empty_name, // issuer
        validity,
        empty_name, // subject
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

fn peer_transport_cfg() -> Arc<TransportConfig> {
    let mut cfg = TransportConfig::default();
    cfg.keep_alive_interval(Some(Duration::from_secs(5)));
    Arc::new(cfg)
}

/// Builds server config for P2P connections.
/// Uses identity-based certs with on-demand signing.
pub fn build_peer_server_cfg(identity: &PeerIdentity) -> Result<ServerConfig> {
    let certified_key = generate_identity_cert(identity)?;

    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(PeerClientCertVerifier))
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    crypto.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    cfg.transport_config(peer_transport_cfg());
    Ok(cfg)
}

/// Builds client config for P2P connections.
/// Uses identity-based certs with on-demand signing.
pub fn build_peer_client_cfg(identity: &PeerIdentity) -> Result<ClientConfig> {
    let certified_key = generate_identity_cert(identity)?;

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerServerCertVerifier))
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(peer_transport_cfg());

    Ok(client)
}

/// Extracts the Ed25519 identity public key from a peer's certificate.
///
/// Performs three checks:
///   1. The cert is a parsable X.509 cert.
///   2. The SPKI algorithm OID is Ed25519 (1.3.101.112).
///   3. The cert is *self-signed* with that SPKI key. Without (3), an attacker
///      who controlled the TLS handshake (a la the old skip-everything verifier)
///      could substitute a cert whose embedded SPKI is some target's key while
///      the cert is actually signed by the attacker's key. Even with the
///      stricter handshake verifier in this file we keep this check as
///      defense-in-depth and to anchor the peer's identity on the SPKI alone.
pub fn extract_peer_public_key(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let peer_identity = conn.peer_identity()?;
    let certs = peer_identity.downcast_ref::<Vec<CertificateDer<'static>>>()?;
    let cert_der = certs.first()?;

    extract_ed25519_pubkey_from_cert(cert_der.as_ref())
}

/// Parse + verify the peer cert's Ed25519 SPKI.
///
/// Returns the 32-byte public key only if:
///   - the DER is a valid X.509 cert,
///   - the SPKI algorithm is Ed25519,
///   - the SPKI subjectPublicKey is a 32-byte BIT STRING,
///   - the cert's signature over its TBS is valid under the SPKI key
///     (i.e. the cert is properly self-signed by the embedded key).
fn extract_ed25519_pubkey_from_cert(cert_der: &[u8]) -> Option<[u8; 32]> {
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    let pubkey = ed25519_pubkey_from_parsed(&cert)?;
    verify_self_signature(&cert, &pubkey).ok()?;
    Some(pubkey)
}

/// Cheaper variant used inside the rustls verifier callbacks: only parse +
/// extract the SPKI, do *not* verify the self-signature. The handshake-time
/// verifier already checks the TLS transcript signature against this same
/// SPKI; the application path adds the self-signature check on top.
fn ed25519_pubkey_from_cert_der(cert_der: &[u8]) -> Option<[u8; 32]> {
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    ed25519_pubkey_from_parsed(&cert)
}

fn ed25519_pubkey_from_parsed(cert: &X509Certificate<'_>) -> Option<[u8; 32]> {
    let spki = cert.public_key();

    // RFC 8410: Ed25519 SPKI uses algorithm OID 1.3.101.112 with no parameters.
    if spki.algorithm.algorithm != ED25519_OID {
        return None;
    }

    // subject_public_key is a BIT STRING; for Ed25519 its data is exactly the
    // 32-byte raw public key (no leading 0x00 unused-bits prefix here — that's
    // already stripped by the parser).
    let raw: &[u8] = &spki.subject_public_key.data;
    if raw.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    Some(out)
}

/// Verify the cert's signatureValue is a valid Ed25519 signature over its
/// tbs_certificate, using the embedded SPKI as the verification key.
fn verify_self_signature(cert: &X509Certificate<'_>, pubkey_bytes: &[u8; 32]) -> Result<()> {
    use anyhow::anyhow;

    // The cert's signatureAlgorithm must also be Ed25519.
    if cert.signature_algorithm.algorithm != ED25519_OID {
        return Err(anyhow!("cert signatureAlgorithm is not Ed25519"));
    }

    let tbs_der: &[u8] = cert.tbs_certificate.as_ref();
    let sig_bytes: &[u8] = &cert.signature_value.data;
    let sig_arr: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| anyhow!("Ed25519 cert signature must be 64 bytes"))?;

    let verifying_key = VerifyingKey::from_bytes(pubkey_bytes)
        .map_err(|e| anyhow!("invalid Ed25519 SPKI bytes: {e}"))?;
    let signature = Ed25519Signature::from_bytes(&sig_arr);

    verifying_key
        .verify(tbs_der, &signature)
        .map_err(|e| anyhow!("cert self-signature did not verify: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer as _;
    use ed25519_dalek::SigningKey;

    /// Build a minimal Ed25519 self-signed cert using the same hand-rolled DER
    /// builder as production, but signed with an explicit caller-supplied key
    /// (so the test doesn't need the JNI-backed `IdentitySigner`).
    fn build_self_signed(signing_key: &SigningKey) -> Vec<u8> {
        let pubkey = signing_key.verifying_key();
        let pubkey_bytes = pubkey.to_bytes();
        let tbs = build_tbs_certificate(&pubkey_bytes);
        let sig = signing_key.sign(&tbs);
        build_certificate_der(&tbs, &sig.to_bytes())
    }

    #[test]
    fn parses_and_verifies_self_signed_cert() {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let key = SigningKey::from_bytes(&seed);
        let cert_der = build_self_signed(&key);

        let parsed = extract_ed25519_pubkey_from_cert(&cert_der)
            .expect("self-signed cert must round-trip through parser+self-sig");
        assert_eq!(parsed, key.verifying_key().to_bytes());
    }

    #[test]
    fn rejects_cert_with_tampered_spki() {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(11).wrapping_add(1);
        }
        let key = SigningKey::from_bytes(&seed);
        let cert_der = build_self_signed(&key);

        // Find the SPKI key bytes inside the DER and flip a bit. After this,
        // the embedded SPKI no longer matches the key the cert was signed
        // with, so self-sig verification must fail.
        let spki_pattern: &[u8] = &[
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        let pos = cert_der
            .windows(spki_pattern.len())
            .position(|w| w == spki_pattern)
            .expect("SPKI pattern present in our generated cert");
        let key_start = pos + spki_pattern.len();
        let mut tampered = cert_der.clone();
        tampered[key_start] ^= 0x01;

        assert!(extract_ed25519_pubkey_from_cert(&tampered).is_none());
    }

    #[test]
    fn rejects_garbage_bytes() {
        assert!(extract_ed25519_pubkey_from_cert(&[]).is_none());
        assert!(extract_ed25519_pubkey_from_cert(&[0u8; 32]).is_none());
        assert!(extract_ed25519_pubkey_from_cert(&[0x30, 0x82, 0xff, 0xff]).is_none());
    }
}
