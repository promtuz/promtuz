//! Node enrollment: load-or-create the single Ed25519 key, validate the
//! CA-issued cert, or emit a CSR and wait. Shared by relay and resolver.

use std::path::Path;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;

use crate::quic::config::load_root_ca;
use crate::quic::id::NodeId;

/// Pull the 32-byte Ed25519 SPKI out of a DER cert. Ed25519 leaf certs carry
/// exactly one `03 21 00` (33-byte) BIT STRING — the pubkey (the signature is
/// `03 41 00`). Mirrors the hand-rolled DER in [`crate::quic::config`].
pub fn spki_ed25519(cert_der: &[u8]) -> Option<[u8; 32]> {
    let needle = [0x03, 0x21, 0x00];
    let pos = cert_der.windows(3).position(|w| w == needle)? + 3;
    cert_der.get(pos..pos + 32)?.try_into().ok()
}

fn first_cert_der(cert_path: &Path) -> Option<CertificateDer<'static>> {
    let pem = std::fs::read(cert_path).ok()?;
    let mut rd = std::io::BufReader::new(&pem[..]);
    rustls_pemfile::certs(&mut rd).flatten().next()
}

/// True iff `cert_path` exists, chains to `ca_path`, is unexpired, names
/// `node_id`, and its SPKI is our `key_pub` (i.e. it is *our* cert).
///
/// Requires the process crypto provider to be installed; the caller
/// ([`ensure_enrolled`]) does this via `setup_crypto_provider`.
pub fn cert_is_valid(
    cert_path: &Path, ca_path: &Path, node_id: &NodeId, key_pub: &[u8; 32],
) -> bool {
    let Some(leaf) = first_cert_der(cert_path) else {
        return false;
    };

    // It must certify *our* key, not just any CA-signed key.
    if spki_ed25519(leaf.as_ref()).as_ref() != Some(key_pub) {
        return false;
    }

    let Ok(roots) = load_root_ca(&ca_path.to_path_buf()) else {
        return false;
    };
    let Ok(verifier) = WebPkiServerVerifier::builder(Arc::new(roots)).build() else {
        return false;
    };
    let Ok(server_name) = ServerName::try_from(node_id.to_string()) else {
        return false;
    };

    verifier
        .verify_server_cert(&leaf, &[], &server_name, &[], UnixTime::now())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_cert() {
        let id = NodeId::new(&[7u8; 32]);
        assert!(!cert_is_valid(
            Path::new("/nonexistent.crt"),
            Path::new("/nonexistent_ca.pem"),
            &id,
            &[7u8; 32],
        ));
    }
}
