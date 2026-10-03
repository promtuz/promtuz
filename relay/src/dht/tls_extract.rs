//! Ed25519 keys and capabilities from peer certificates. Inbound `peer/5` peers present no client
//! certificate, so their key comes from the verified `DhtHello` instead.

use common::node::capability::NodeCapabilities;
use common::node::enroll::cert_capabilities;
use thiserror::Error;
use x509_parser::der_parser::Oid;
use x509_parser::oid_registry::asn1_rs::oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

use common::quic::id::NodeId;

const ED25519_OID: Oid<'static> = oid!(1.3.101 .112);

/// The CA-signed [`NodeCapabilities`] in a dialed peer's leaf cert, if it carries the extension.
pub(crate) fn capabilities_from_conn(conn: &quinn::Connection) -> Option<NodeCapabilities> {
    let identity = conn.peer_identity()?;
    let chain = identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()?;
    cert_capabilities(chain.first()?)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ExtractError {
    #[error("peer_identity() absent (no client cert under with_no_client_auth?)")]
    NoCertChain,

    #[error("peer_identity() returned unexpected payload type")]
    UnexpectedIdentityType,

    #[error("peer cert chain is empty")]
    EmptyChain,

    #[error("leaf cert is not a parsable X.509")]
    MalformedCert,

    #[error("leaf cert SPKI is not Ed25519")]
    NotEd25519,

    #[error("leaf cert SPKI subject_public_key is not 32 bytes")]
    BadSpkiLength,

    #[error("BLAKE3(spki) does not match claimed NodeId")]
    NodeIdMismatch,
}

/// The `BLAKE3(SPKI) == NodeId` pin. The `peer/5` dialer accepts any well-formed Ed25519 cert with
/// no CA chain check, so this is the only identity check on an outbound dial.
pub(crate) fn extract_and_verify_pubkey(
    conn: &quinn::Connection, claimed: &NodeId,
) -> Result<[u8; 32], ExtractError> {
    let pubkey = extract_pubkey_from_conn(conn)?;
    let derived = NodeId::new(pubkey);
    if derived != *claimed {
        return Err(ExtractError::NodeIdMismatch);
    }
    Ok(pubkey)
}

fn extract_pubkey_from_conn(conn: &quinn::Connection) -> Result<[u8; 32], ExtractError> {
    let identity = conn.peer_identity().ok_or(ExtractError::NoCertChain)?;
    let chain = identity
        .downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok_or(ExtractError::UnexpectedIdentityType)?;
    let leaf = chain.first().ok_or(ExtractError::EmptyChain)?;
    extract_pubkey_from_leaf_der(leaf.as_ref())
}

fn extract_pubkey_from_leaf_der(der: &[u8]) -> Result<[u8; 32], ExtractError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|_| ExtractError::MalformedCert)?;
    let spki = cert.public_key();
    if spki.algorithm.algorithm != ED25519_OID {
        return Err(ExtractError::NotEd25519);
    }
    spki.subject_public_key.data.as_ref().try_into().map_err(|_| ExtractError::BadSpkiLength)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `peer/5` dial pins `BLAKE3(SPKI)` to the NodeId and checks nothing else, so only a
    /// well-formed Ed25519 leaf may yield a key.
    #[test]
    fn only_a_well_formed_ed25519_leaf_yields_a_key() {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let peer_cert = common::quic::config::build_self_signed_ed25519_cert(signing.clone()).cert;
        let der = peer_cert[0].as_ref();
        let ecdsa = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap().cert;
        for (label, leaf, expected) in [
            ("peer certificate", der, Ok(signing.verifying_key().to_bytes())),
            ("another algorithm", ecdsa.der().as_ref(), Err(ExtractError::NotEd25519)),
            ("truncated", &der[..der.len() / 2], Err(ExtractError::MalformedCert)),
            ("garbage", b"not a certificate".as_slice(), Err(ExtractError::MalformedCert)),
        ] {
            assert_eq!(extract_pubkey_from_leaf_der(leaf), expected, "{label}");
        }
    }
}
