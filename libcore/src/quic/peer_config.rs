use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::Signature as Ed25519Signature;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
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
// On top of that, the application performs `verify_self_signature` on the
// peer cert post-handshake (see `extract_peer_tls_pubkey`). That step rejects
// certs whose embedded SPKI does not match the key that signed the cert
// itself, closing the substring-search spoof of the old extractor.
//
// IMPORTANT: the SPKI in a peer's cert is the peer's **TLS sub-key**, not
// their long-term IPK. The IPK is bound to the connection out-of-band by
// the application-level identity exchange (a signature by the IPK over the
// TLS sub-key pubkey, verified before the contact is saved). See the
// `IdentityP::AddMe` flow in `crate::api::identity`.
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
        // by `extract_peer_tls_pubkey` + the application-layer IPK binding.
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

/// **Phase 7 (P0-2)**: pinned-pubkey variant of [`PeerServerCertVerifier`].
///
/// Holds the **expected SPKI bytes** for a single dial; the TLS handshake
/// is rejected unless the presented cert's SPKI matches `expected`. The
/// rest of the verification (Ed25519 sig over the TLS 1.3 transcript)
/// runs the same as the un-pinned verifier.
///
/// The pin is the libcore caller's defense against a network MitM:
/// `Peer1DhtClient` knows which NodeKey pubkey it expects for each home
/// (vended by the resolver via `RelayDescriptor.pubkey` and persisted on
/// `data::relay::Relay::pubkey`); the verifier's job is to ensure the
/// cert SPKI matches that value before any application bytes flow.
///
/// Built per-dial (not shared across the pool) so each dial pins exactly
/// the target it intends to talk to.
#[derive(Debug)]
pub struct PinnedPeerServerCertVerifier {
    pub expected: [u8; 32],
}

impl ServerCertVerifier for PinnedPeerServerCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = ed25519_pubkey_from_cert_der(end_entity.as_ref())
            .ok_or_else(|| rustls::Error::General(
                "peer cert is not a valid Ed25519 X.509".into(),
            ))?;
        if presented != self.expected {
            return Err(rustls::Error::General(format!(
                "peer cert SPKI mismatch: expected {} got {}",
                hex_short(&self.expected),
                hex_short(&presented)
            )));
        }
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

fn hex_short(b: &[u8; 32]) -> String {
    let mut out = String::with_capacity(8);
    for x in &b[..4] {
        out.push_str(&format!("{x:02x}"));
    }
    out
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

// ===========================================================================
// TLS sub-key separation
//
// rustls hands its TLS 1.3 transcript to the configured Signer; whatever key
// signs the application-layer `DispatchP` transcript MUST NOT also sign that
// rustls transcript. Even though the two prefixes differ today, deterministic
// Ed25519 means a single shared key turns any future cross-protocol
// confusion into a permanent identity compromise. We therefore derive a
// stable, per-identity TLS sub-key (HKDF-SHA256, info `promtuz-p2p-tls-v1`,
// salt = IPK pub bytes; see `common::crypto::sign::derive_p2p_tls_key`),
// embed *its* pubkey in the cert SPKI, and sign the cert plus all TLS-layer
// signatures with it. The long-term IPK never reaches rustls.
//
// The IPK<->TLS-subkey binding for the application's "who is this peer?"
// question is carried at the application layer (option (b) in the design
// notes): the identity-exchange handshake includes the IPK + an Ed25519
// signature by the IPK over the TLS sub-key pubkey. Receivers verify and
// only then save the contact under the IPK. See
// [`crate::api::identity`] for that flow.
// ===========================================================================

/// Custom rustls SigningKey backed by a cached TLS sub-key.
///
/// We cache the derived sub-key on the struct so we don't have to hit the
/// JNI/key-manager path (which decrypts the long-term IPK) on every TLS
/// handshake signature. The sub-key public component is what ends up in the
/// cert SPKI handed to the peer; the long-term IPK never enters rustls.
#[derive(Debug)]
struct IdentitySigningKey {
    /// Public component of the derived TLS sub-key — this is what the peer
    /// sees in the cert SPKI and in `verify_tls13_signature`.
    public_key: VerifyingKey,
    /// Shared cache of the derived sub-key. `Arc` so cloning into the
    /// per-handshake `IdentityTlsSigner` is cheap; `SigningKey` self-wipes
    /// on drop (via the workspace's `ed25519-dalek` `zeroize` feature) so
    /// the secret bytes are wiped when the last reference drops.
    subkey:     Arc<SigningKey>,
}

impl rustls::sign::SigningKey for IdentitySigningKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&SignatureScheme::ED25519) {
            Some(Box::new(IdentityTlsSigner { subkey: Arc::clone(&self.subkey) }))
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

/// Per-handshake rustls Signer that signs with the derived TLS sub-key.
///
/// rustls constructs a fresh `Signer` per handshake via `choose_scheme`; we
/// hand it a cheap clone of the cached sub-key `Arc` so signing stays
/// allocation-free on the hot path.
#[derive(Debug)]
struct IdentityTlsSigner {
    subkey: Arc<SigningKey>,
}

impl rustls::sign::Signer for IdentityTlsSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        Ok(self.subkey.sign(message).to_bytes().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// Generate an X.509 self-signed cert for this identity's TLS sub-key.
///
/// The SPKI embedded in the cert is the **TLS sub-key's** public bytes (NOT
/// the long-term IPK). The cert's signature is also produced by the TLS
/// sub-key, so `extract_ed25519_pubkey_from_cert`'s self-signature check
/// passes against the embedded SPKI without ever touching the IPK.
///
/// `_identity` is currently unused — the SPKI/sub-key is derived from the
/// IPK by `IdentitySigner::tls_subkey()` rather than passed in. The
/// argument is kept for call-site symmetry and so future versions can carry
/// per-identity options through it.
fn generate_identity_cert(_identity: &PeerIdentity) -> Result<CertifiedKey> {
    let subkey = IdentitySigner::tls_subkey()?;
    let subkey_arc = Arc::new(subkey);
    let public_key = subkey_arc.verifying_key();

    // Self-sign the TBS with the TLS sub-key so cert SPKI ↔ cert sig key
    // match. Anything else would break the post-handshake self-signature
    // check in `extract_ed25519_pubkey_from_cert`.
    let tbs = build_tbs_certificate(public_key.as_bytes());
    let signature = subkey_arc.sign(&tbs);

    let cert_der = build_certificate_der(&tbs, &signature.to_bytes());
    let certs = vec![CertificateDer::from(cert_der)];

    let signing_key: Arc<dyn rustls::sign::SigningKey> =
        Arc::new(IdentitySigningKey { public_key, subkey: subkey_arc });

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

/// **Phase 7 (P0-2)**: build a `peer/1` client config that pins the
/// expected server SPKI to `expected`. Otherwise identical to
/// [`build_peer_client_cfg`]; use this when the dialer knows which
/// relay pubkey it should be talking to (e.g. resolver-vended via
/// `RelayDescriptor.pubkey`). The dial fails if the relay presents a
/// different SPKI, defeating a same-network MitM.
pub fn build_pinned_peer_client_cfg(
    identity: &PeerIdentity, expected: [u8; 32],
) -> Result<ClientConfig> {
    let certified_key = generate_identity_cert(identity)?;

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedPeerServerCertVerifier { expected }))
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(peer_transport_cfg());

    Ok(client)
}

/// Phase 5b: build a `peer/1` client config from an explicit Ed25519
/// TLS sub-key, bypassing the global `Identity::tls_subkey()` lookup.
/// Used by the e2e harness to instantiate independent libcore-flavoured
/// clients in-process. Production callers continue to use
/// [`build_peer_client_cfg`] which derives the sub-key from the libcore-
/// global IPK secret.
pub fn build_peer_client_cfg_with_subkey(subkey: ed25519_dalek::SigningKey) -> Result<ClientConfig> {
    let certified_key = generate_identity_cert_with_subkey(subkey);

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

/// **Phase 7 (P0-2)**: `(subkey, expected)` flavour of
/// [`build_pinned_peer_client_cfg`]. The dialer
/// (`Peer1DhtClient::build_pinned_cfg`) calls this on every per-dial
/// build because it holds the TLS sub-key explicitly rather than
/// going through the global `IdentitySigner::tls_subkey()`.
pub fn build_pinned_peer_client_cfg_with_subkey(
    subkey: ed25519_dalek::SigningKey, expected: [u8; 32],
) -> Result<ClientConfig> {
    let certified_key = generate_identity_cert_with_subkey(subkey);

    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedPeerServerCertVerifier { expected }))
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(certified_key)));

    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(peer_transport_cfg());

    Ok(client)
}

/// Phase 5b: identity-cert builder that takes the TLS sub-key directly
/// (skipping the `IdentitySigner::tls_subkey()` global lookup).
/// Same DER-encoding path as [`generate_identity_cert`].
fn generate_identity_cert_with_subkey(subkey: ed25519_dalek::SigningKey) -> CertifiedKey {
    use ed25519_dalek::Signer;
    let subkey_arc = Arc::new(subkey);
    let public_key = subkey_arc.verifying_key();

    let tbs = build_tbs_certificate(public_key.as_bytes());
    let signature = subkey_arc.sign(&tbs);

    let cert_der = build_certificate_der(&tbs, &signature.to_bytes());
    let certs = vec![CertificateDer::from(cert_der)];

    let signing_key: Arc<dyn rustls::sign::SigningKey> =
        Arc::new(IdentitySigningKey { public_key, subkey: subkey_arc });

    CertifiedKey::new(certs, signing_key)
}

/// Extracts the peer's **TLS sub-key** Ed25519 public key from their cert.
///
/// Note: this is the *TLS-layer* identity, not the user's long-term IPK.
/// The IPK is bound to the connection at the application layer via the
/// identity-exchange protocol (see `IdentityP::AddMe` and
/// `verify_ipk_binding`). Storing this key as a contact identifier would
/// be a bug — always pair it with the verified IPK from the app layer.
///
/// Performs three checks:
///   1. The cert is a parsable X.509 cert.
///   2. The SPKI algorithm OID is Ed25519 (1.3.101.112).
///   3. The cert is *self-signed* with that SPKI key. Without (3), an attacker
///      who controlled the TLS handshake (a la the old skip-everything verifier)
///      could substitute a cert whose embedded SPKI is some target's key while
///      the cert is actually signed by the attacker's key. Even with the
///      stricter handshake verifier in this file we keep this check as
///      defense-in-depth and to anchor the peer's TLS-layer identity on the
///      SPKI alone.
pub fn extract_peer_tls_pubkey(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let peer_identity = conn.peer_identity()?;
    let certs = peer_identity.downcast_ref::<Vec<CertificateDer<'static>>>()?;
    let cert_der = certs.first()?;

    extract_ed25519_pubkey_from_cert(cert_der.as_ref())
}

/// Verify that `ipk` claims authorship of `tls_pubkey` by signing the
/// canonical binding transcript. This is the application-layer half of the
/// identity-key separation: cert SPKI carries the TLS sub-key, this proof
/// carries the long-term IPK.
///
/// Used by both ends of `IdentityP::AddMe` after TLS comes up to decide
/// what IPK to store the contact under.
pub fn verify_ipk_binding(
    ipk: &[u8; 32], tls_pubkey: &[u8; 32], sig: &[u8; 64],
) -> Result<(), rustls::Error> {
    let vk = VerifyingKey::from_bytes(ipk)
        .map_err(|e| rustls::Error::General(format!("invalid IPK bytes: {e}")))?;
    let signature = Ed25519Signature::from_bytes(sig);
    let msg = ipk_binding_message(tls_pubkey);
    vk.verify_strict(&msg, &signature)
        .map_err(|e| rustls::Error::General(format!("IPK binding signature failed: {e}")))
}

/// Canonical transcript for "this IPK authorizes that TLS sub-key".
///
/// Bumping the prefix rotates the binding format; keep stable across
/// releases for backwards compatibility with already-saved contacts.
pub fn ipk_binding_message(tls_pubkey: &[u8; 32]) -> [u8; 64] {
    const PREFIX: &[u8; 32] = b"promtuz-ipk-tls-binding-v1......";
    debug_assert_eq!(PREFIX.len(), 32);
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(PREFIX);
    out[32..].copy_from_slice(tls_pubkey);
    out
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

    #[test]
    fn derive_p2p_tls_key_is_deterministic_and_distinct_from_identity() {
        use common::crypto::sign::derive_p2p_tls_key;
        let seed = [7u8; 32];
        let ipk = SigningKey::from_bytes(&seed);
        let ipk_pub = ipk.verifying_key().to_bytes();

        let sub_a = derive_p2p_tls_key(&seed, &ipk_pub);
        let sub_b = derive_p2p_tls_key(&seed, &ipk_pub);
        assert_eq!(sub_a.to_bytes(), sub_b.to_bytes(), "derivation must be deterministic");

        // The whole point: the TLS sub-key must NOT equal the long-term IPK.
        assert_ne!(
            sub_a.to_bytes(),
            seed,
            "TLS sub-key seed must differ from identity seed"
        );
        assert_ne!(
            sub_a.verifying_key().to_bytes(),
            ipk_pub,
            "TLS sub-key pubkey must differ from IPK pubkey"
        );
    }

    #[test]
    fn ipk_binding_roundtrips() {
        // Simulates what `IdentitySigner::sign_with_ipk` + `verify_ipk_binding`
        // do at the application layer to bind a TLS sub-key to an IPK.
        let ipk = SigningKey::from_bytes(&[3u8; 32]);
        let ipk_pub = ipk.verifying_key().to_bytes();
        let tls_pub = SigningKey::from_bytes(&[9u8; 32]).verifying_key().to_bytes();

        let msg = ipk_binding_message(&tls_pub);
        let sig = ipk.sign(&msg).to_bytes();

        verify_ipk_binding(&ipk_pub, &tls_pub, &sig).expect("binding must verify");

        // Tampered TLS pubkey: must fail.
        let mut other_tls = tls_pub;
        other_tls[0] ^= 0x01;
        assert!(verify_ipk_binding(&ipk_pub, &other_tls, &sig).is_err());

        // Wrong claimed IPK: must fail.
        let other_ipk = SigningKey::from_bytes(&[4u8; 32]).verifying_key().to_bytes();
        assert!(verify_ipk_binding(&other_ipk, &tls_pub, &sig).is_err());
    }

    #[test]
    fn cert_built_with_subkey_self_verifies() {
        // The production cert is signed with the TLS sub-key and embeds the
        // sub-key's pubkey in the SPKI. The post-handshake extractor MUST
        // accept that pairing — this test guards against accidentally
        // re-introducing IPK-as-SPKI signing.
        use common::crypto::sign::derive_p2p_tls_key;
        let ipk_seed = [42u8; 32];
        let ipk_pub = SigningKey::from_bytes(&ipk_seed).verifying_key().to_bytes();
        let subkey = derive_p2p_tls_key(&ipk_seed, &ipk_pub);

        let cert_der = build_self_signed(&subkey);

        let parsed = extract_ed25519_pubkey_from_cert(&cert_der)
            .expect("subkey-signed cert must verify against subkey SPKI");
        assert_eq!(parsed, subkey.verifying_key().to_bytes());
        assert_ne!(parsed, ipk_pub, "cert SPKI must be the sub-key, not the IPK");
    }
}
