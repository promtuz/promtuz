use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use common::crypto::verify_ed25519;
use common::quic::config::Ed25519CertVerifier;
use quinn::ClientConfig;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use x509_parser::oid_registry::asn1_rs::oid;
use x509_parser::prelude::FromDer;
use x509_parser::prelude::X509Certificate;

const ED25519_OID: x509_parser::der_parser::Oid<'static> = oid!(1.3.101 .112);

fn peer_transport_cfg() -> Arc<TransportConfig> {
    let mut cfg = TransportConfig::default();
    cfg.keep_alive_interval(Some(Duration::from_secs(5)));
    Arc::new(cfg)
}

pub fn build_peer_server_cfg(key: Arc<CertifiedKey>, alpns: Vec<Vec<u8>>) -> Result<ServerConfig> {
    let mut crypto = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(Ed25519CertVerifier))
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(key)));

    crypto.alpn_protocols = alpns;

    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(crypto)?));
    cfg.transport_config(peer_transport_cfg());
    // The punch socket swaps the real path under a fixed synthetic address, so a datagram that
    // arrives before its source is relabeled must be dropped, not taken as a migration.
    cfg.migration(false);
    Ok(cfg)
}

pub fn build_peer_client_cfg(key: Arc<CertifiedKey>, alpns: Vec<Vec<u8>>) -> Result<ClientConfig> {
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Ed25519CertVerifier))
        .with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(key)));

    tls.alpn_protocols = alpns;

    let quic_config = QuicClientConfig::try_from(tls)?;

    let mut client = ClientConfig::new(Arc::new(quic_config));
    client.transport_config(peer_transport_cfg());

    Ok(client)
}

/// The peer's TLS sub-key, never an identity on its own: `transfer::auth` binds it to an IPK.
/// The cert must also be self-signed by that key, as defense in depth.
pub fn extract_peer_tls_pubkey(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let peer_identity = conn.peer_identity()?;
    let certs = peer_identity.downcast_ref::<Vec<CertificateDer<'static>>>()?;
    let cert_der = certs.first()?;

    extract_ed25519_pubkey_from_cert(cert_der.as_ref())
}

/// Keep the prefix stable: peers on other versions sign and verify this exact transcript.
pub fn ipk_binding_message(tls_pubkey: &[u8; 32]) -> [u8; 64] {
    const PREFIX: &[u8; 32] = b"promtuz-ipk-tls-binding-v1......";
    debug_assert_eq!(PREFIX.len(), 32);
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(PREFIX);
    out[32..].copy_from_slice(tls_pubkey);
    out
}

fn extract_ed25519_pubkey_from_cert(cert_der: &[u8]) -> Option<[u8; 32]> {
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    let spki = cert.public_key();
    if spki.algorithm.algorithm != ED25519_OID || cert.signature_algorithm.algorithm != ED25519_OID {
        return None;
    }
    // The parser strips the BIT STRING's unused-bits byte, leaving the raw 32-byte key.
    let pubkey: [u8; 32] = spki.subject_public_key.data.as_ref().try_into().ok()?;
    let sig = cert.signature_value.data.as_ref().try_into().ok()?;
    verify_ed25519(&pubkey, cert.tbs_certificate.as_ref(), sig).ok()?;
    Some(pubkey)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;

    fn cert(seed: [u8; 32]) -> (Vec<u8>, [u8; 32]) {
        let key = SigningKey::from_bytes(&seed);
        let certified = common::quic::config::build_self_signed_ed25519_cert(key.clone());
        (certified.cert[0].to_vec(), key.verifying_key().to_bytes())
    }

    #[test]
    fn a_peer_certificate_yields_its_key_only_when_that_key_signed_it() {
        let mut leads = std::collections::HashSet::new();
        for n in 0u16.. {
            let mut seed = [7; 32];
            seed[..2].copy_from_slice(&n.to_le_bytes());
            let (der, key) = cert(seed);
            assert_eq!(extract_ed25519_pubkey_from_cert(&der), Some(key), "seed {n}");
            leads.insert(key[0]);
            if leads.len() == 256 {
                break;
            }
        }
        let (der, key) = cert([3; 32]);
        let at = der.windows(32).position(|w| w == key).unwrap();
        let mut tampered = der.clone();
        tampered[at] ^= 1;
        assert_eq!(
            extract_ed25519_pubkey_from_cert(&tampered),
            None,
            "the signature covers the key"
        );
        for garbage in [&[][..], &[0; 32], &[0x30, 0x82, 0xff, 0xff]] {
            assert_eq!(extract_ed25519_pubkey_from_cert(garbage), None);
        }
    }
}
