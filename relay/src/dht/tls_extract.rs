//! Post-handshake TLS pubkey extraction for `peer/1` connections.
//!
//! ## Purpose
//!
//! Every relay-to-relay connection MUST be bound to a verified Ed25519
//! pubkey before any DHT RPC is honoured against it.
//! The relay's TLS certs are CA-signed (per `common/src/bin/certgen.rs`),
//! and the leaf cert SPKI is the relay's identity Ed25519 pubkey, with
//! `BLAKE3(spki) == NodeId`.
//!
//! The post-handshake check here is **defense-in-depth**: rustls has
//! already validated the cert chain against the configured root CA
//! during the handshake. We then:
//!
//! 1. Extract the leaf cert from `Connection::peer_identity()`.
//! 2. Parse it as X.509 via `x509-parser` (same crate as
//!    `libcore/src/quic/peer_config.rs`).
//! 3. Pull out the Ed25519 SPKI (32 bytes) — verifying the algorithm
//!    OID is `1.3.101.112` (`id-Ed25519`).
//! 4. (Caller) verify `BLAKE3(spki) == claimed_node_id` and reject
//!    on mismatch.
//!
//! ## Inbound vs outbound
//!
//! - **Outbound** (we are the QUIC dialer / peer/1 client): the
//!   server's leaf cert is available via `peer_identity()` because
//!   server cert verification is part of any TLS handshake. This is
//!   the path used by `lookup::connect_to_peer`.
//!
//! - **Inbound** (we are the QUIC server accepting peer/1): the
//!   relay's QUIC server config currently uses `with_no_client_auth()`
//!   (see `common/src/quic/config.rs::build_server_cfg`), so the
//!   client (the dialing peer) does NOT present a client cert and
//!   `Connection::peer_identity()` returns `None`. There is no
//!   leaf-cert SPKI to extract. The function below detects this case
//!   and returns `Err(ExtractError::NoCertChain)`; callers handle it
//!   by recording a `[0u8; 32]` placeholder pubkey (and accepting that
//!   the inbound peer's identity is application-layer-derived, not
//!   cert-pinned).
//!
//! Closing the inbound-side gap requires switching `peer/1` server
//! config to mTLS (`with_client_cert_verifier(...)`), which lives in
//! `common/src/quic/config.rs` — explicitly out of scope for this
//! dispatch. Documented as a follow-up.
//!
//! ## Why we don't reuse libcore's helper directly
//!
//! `libcore/src/quic/peer_config.rs::extract_ed25519_pubkey_from_cert`
//! verifies a *self-signed* cert (its self-signature against its own
//! SPKI). The relay's TLS leaf is **CA-signed**, not self-signed, so
//! `verify_self_signature` fails on a legitimate relay cert. The
//! relay-side parser below skips the self-signature step — the
//! handshake-level CA chain validation has already authenticated the
//! cert, so we only need to extract the SPKI.
//!

use common::node::capability::CAPABILITY_OID;
use common::node::capability::NodeCapabilities;
use thiserror::Error;
use x509_parser::der_parser::Oid;
use x509_parser::oid_registry::asn1_rs::oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

use common::quic::id::NodeId;

/// Ed25519 SPKI algorithm OID per RFC 8410.
const ED25519_OID: Oid<'static> = oid!(1.3.101 .112);

/// Read the CA-signed [`NodeCapabilities`] from a connection's peer leaf cert,
/// if the capability extension is present. Used to check a dialed gateway
/// actually carries `PUSH_GATEWAY` before trusting it with a wake — the
/// resolver directory is untrusted. Works because the dialed peer is the TLS
/// *server* and always presents its cert (no client-auth needed).
pub(crate) fn capabilities_from_conn(conn: &quinn::Connection) -> Option<NodeCapabilities> {
    let identity = conn.peer_identity()?;
    let chain = identity.downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()?;
    capabilities_from_leaf_der(chain.first()?.as_ref())
}

fn capabilities_from_leaf_der(der: &[u8]) -> Option<NodeCapabilities> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    let oid = Oid::from(CAPABILITY_OID).ok()?;
    let ext = cert.extensions().iter().find(|e| e.oid == oid)?;
    NodeCapabilities::decode(ext.value)
}

/// Reasons the post-handshake TLS pubkey extraction can fail. Each
/// maps to a `CloseReason` at the call site (currently always
/// `DhtMalformedKey`, but separating the cases gives operator-friendly
/// log lines).
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ExtractError {
    /// `Connection::peer_identity()` returned `None`. On the inbound
    /// path this is the expected outcome under `with_no_client_auth()`
    /// (see module docs); the caller treats it as "use the placeholder
    /// pubkey".
    #[error("peer_identity() absent (no client cert under with_no_client_auth?)")]
    NoCertChain,

    /// `peer_identity()` returned an unexpected payload type. Should
    /// not occur for any rustls-backed connection; bug-defensive guard.
    #[error("peer_identity() returned unexpected payload type")]
    UnexpectedIdentityType,

    /// Cert chain was empty (downcast succeeded but the `Vec` was
    /// empty). Cannot happen post-handshake under any sane TLS config.
    #[error("peer cert chain is empty")]
    EmptyChain,

    /// X.509 parse failed on the leaf cert.
    #[error("leaf cert is not a parsable X.509")]
    MalformedCert,

    /// Leaf cert SPKI algorithm is not Ed25519 (`1.3.101.112`).
    #[error("leaf cert SPKI is not Ed25519")]
    NotEd25519,

    /// SPKI subject_public_key BIT STRING is not 32 bytes (Ed25519
    /// pubkeys are exactly 32 bytes per RFC 8410).
    #[error("leaf cert SPKI subject_public_key is not 32 bytes")]
    BadSpkiLength,

    /// `BLAKE3(spki) != claimed_node_id`. Caller-supplied
    /// `claimed_node_id` is the SNI / outbound-dial target identity;
    /// a mismatch is a Sybil-spoofing red flag.
    #[error("BLAKE3(spki) does not match claimed NodeId")]
    NodeIdMismatch,
}

/// Extract the verified Ed25519 SPKI from a peer-side TLS cert chain
/// and check that `BLAKE3(spki) == claimed`. Returns the 32-byte SPKI
/// on success.
///
/// Used by [`crate::dht::lookup::connect_to_peer`] post-`connect()`
/// to authenticate the *outbound* dial target. The TLS layer has
/// already chain-validated against the configured root CA; this is
/// belt-and-suspenders against a CA that minted a misbinding cert.
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

/// Extract the Ed25519 SPKI from the leaf cert of a connection's peer
/// identity. Does NOT do the `BLAKE3(spki) == NodeId` check —
/// [`extract_and_verify_pubkey`] is the binding-checking entry point.
///
/// Exposed for callers that want the raw pubkey without an a-priori
/// NodeId to compare against (e.g. the inbound-path landing zone, if
/// mTLS is ever enabled).
fn extract_pubkey_from_conn(conn: &quinn::Connection) -> Result<[u8; 32], ExtractError> {
    let identity = conn.peer_identity().ok_or(ExtractError::NoCertChain)?;
    let chain = identity
        .downcast_ref::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok_or(ExtractError::UnexpectedIdentityType)?;
    let leaf = chain.first().ok_or(ExtractError::EmptyChain)?;
    extract_pubkey_from_leaf_der(leaf.as_ref())
}

/// X.509 parse + Ed25519 SPKI extraction. No self-signature check —
/// the relay's leaf is CA-signed and was already validated at the TLS
/// layer. Exposed for unit tests so we don't need a real QUIC
/// `Connection` to exercise the parsing path.
pub(crate) fn extract_pubkey_from_leaf_der(der: &[u8]) -> Result<[u8; 32], ExtractError> {
    let (_, cert) =
        X509Certificate::from_der(der).map_err(|_| ExtractError::MalformedCert)?;
    let spki = cert.public_key();
    if spki.algorithm.algorithm != ED25519_OID {
        return Err(ExtractError::NotEd25519);
    }
    let raw: &[u8] = &spki.subject_public_key.data;
    if raw.len() != 32 {
        return Err(ExtractError::BadSpkiLength);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(raw);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Ed25519-self-signed cert for the parser test.
    /// (We use self-signed only because it's easier to build by hand
    /// in a test — the parser doesn't care about self-vs-CA-signed,
    /// it only inspects the SPKI.)
    fn build_test_cert(pubkey: &[u8; 32], signature: &[u8; 64]) -> Vec<u8> {
        let ed25519_oid: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
        let spki = [
            &[0x30, 0x2a][..],
            &[0x30, 0x05][..],
            ed25519_oid,
            &[0x03, 0x21, 0x00][..],
            pubkey,
        ]
        .concat();

        let serial = &pubkey[0..8];
        let validity: &[u8] = &[
            0x30, 0x1e, 0x17, 0x0d, b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0',
            b'0', b'0', b'Z', 0x17, 0x0d, b'5', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0',
            b'0', b'0', b'0', b'Z',
        ];
        let empty_name: &[u8] = &[0x30, 0x00];
        let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];
        let serial_der = [&[0x02, serial.len() as u8][..], serial].concat();
        let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

        let tbs_content = [
            version,
            &serial_der,
            sig_alg,
            empty_name,
            validity,
            empty_name,
            &spki,
        ]
        .concat();

        let tbs = encode_seq(&tbs_content);
        let sig_bitstring = [&[0x03, 0x41, 0x00][..], signature].concat();
        let cert_content = [&tbs[..], sig_alg, &sig_bitstring].concat();
        encode_seq(&cert_content)
    }

    fn encode_seq(data: &[u8]) -> Vec<u8> {
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

    #[test]
    fn extract_pubkey_round_trip() {
        // Valid Ed25519 cert → returns the SPKI bytes.
        let pubkey = [0x42u8; 32];
        let der = build_test_cert(&pubkey, &[0u8; 64]);
        let extracted = extract_pubkey_from_leaf_der(&der).expect("parse");
        assert_eq!(extracted, pubkey);
    }

    #[test]
    fn extract_pubkey_garbage_fails() {
        assert_eq!(
            extract_pubkey_from_leaf_der(&[]),
            Err(ExtractError::MalformedCert)
        );
        assert_eq!(
            extract_pubkey_from_leaf_der(&[0x30, 0x82, 0xff, 0xff]),
            Err(ExtractError::MalformedCert)
        );
    }

}
