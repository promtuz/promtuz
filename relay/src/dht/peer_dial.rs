//! `peer/1` outbound dial config + its TLS verifier.
//!
//! Relay↔relay (`peer/1`) is the **key-as-identity** trust domain, distinct
//! from the CA-hierarchical client/resolver/relay-control certs: a relay's
//! identity *is* its NodeKey, and `NodeId == BLAKE3(NodeKey)`. Because the
//! NodeKey is deliberately separate from the CA-issued TLS key (`relay/mod`
//! — a TLS-layer compromise must not become an identity compromise), the
//! `peer/1` ALPN serves a self-signed cert whose SPKI is the NodeKey (the
//! ALPN split in `common/src/quic/config.rs`). A CA verifier cannot
//! validate that cert, so this dialer installs a verifier that:
//!
//!   * accepts any well-formed Ed25519 identity cert (no CA chain, no
//!     validity window, no SAN — none apply to a key-as-identity cert), and
//!   * verifies the TLS 1.3 handshake signature under the cert's SPKI,
//!     proving the server holds the private NodeKey.
//!
//! The per-dial `NodeId` pin is done post-handshake by
//! [`super::tls_extract::extract_and_verify_pubkey`] inside
//! [`super::lookup::connect_to_peer`]: a MITM presenting any other identity
//! cert fails `BLAKE3(SPKI) == NodeId` and the dial is dropped. Net trust
//! is SPKI pinning, just deferred past the handshake — the same model
//! libcore's (now-deleted) `Peer1DhtClient` verifier used.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::quic::protorole::ProtoRole;
use ed25519_dalek::Signature as Ed25519Signature;
use ed25519_dalek::Verifier as _;
use ed25519_dalek::VerifyingKey;
use quinn::IdleTimeout;
use quinn::TransportConfig;
use quinn::VarInt;
use quinn::crypto::rustls::QuicClientConfig;
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;

use super::tls_extract::extract_pubkey_from_leaf_der;

/// Verifier for `peer/1` dials: accept any well-formed Ed25519 identity
/// cert and verify the handshake signature under its SPKI. CA chain,
/// validity window, and SAN are intentionally NOT checked — a peer's
/// identity is its key, pinned to the dialed `NodeId` post-handshake.
#[derive(Debug)]
struct PeerServerCertVerifier;

impl ServerCertVerifier for PeerServerCertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // Accept iff it parses as an Ed25519 X.509 (identity cert). The
        // `BLAKE3(SPKI) == NodeId` binding is enforced post-handshake.
        extract_pubkey_from_leaf_der(end_entity.as_ref()).map_err(|e| {
            rustls::Error::General(format!("peer cert is not an Ed25519 X.509: {e}"))
        })?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("peer/1 requires TLS 1.3".into()))
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(rustls::Error::General(format!(
                "unsupported handshake signature scheme: {:?}",
                dss.scheme
            )));
        }
        let pubkey = extract_pubkey_from_leaf_der(cert.as_ref())
            .map_err(|e| rustls::Error::General(format!("peer cert SPKI not Ed25519: {e}")))?;
        let verifying_key = VerifyingKey::from_bytes(&pubkey)
            .map_err(|e| rustls::Error::General(format!("invalid Ed25519 SPKI: {e}")))?;
        let sig: [u8; 64] = dss.signature().try_into().map_err(|_| {
            rustls::Error::General("Ed25519 handshake signature must be 64 bytes".into())
        })?;
        verifying_key
            .verify(message, &Ed25519Signature::from_bytes(&sig))
            .map_err(|e| rustls::Error::General(format!("handshake signature verify failed: {e}")))?;
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Build the `peer/1` outbound [`quinn::ClientConfig`] — identity-cert
/// verifier + the `peer` ALPN. Transport settings mirror the private
/// `common::quic::config::default_client_transport`.
pub(crate) fn build_peer_client_cfg() -> Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PeerServerCertVerifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic = QuicClientConfig::try_from(tls)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));

    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is a valid IdleTimeout"),
    ));
    tc.keep_alive_interval(Some(Duration::from_secs(10)));
    tc.max_concurrent_bidi_streams(VarInt::from_u32(64));
    tc.max_concurrent_uni_streams(VarInt::from_u32(64));
    cfg.transport_config(Arc::new(tc));

    Ok(cfg)
}
