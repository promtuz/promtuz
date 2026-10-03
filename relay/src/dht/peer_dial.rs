//! `peer/N` dial config. On `peer/N` a relay presents a self-signed cert for its NodeKey
//! (`NodeId == BLAKE3(NodeKey)`), which a CA verifier cannot validate.

use std::sync::Arc;

use anyhow::Result;
use common::quic::config::Ed25519CertVerifier;
use common::quic::config::default_client_transport;
use common::quic::protorole::ProtoRole;
use quinn::crypto::rustls::QuicClientConfig;

pub(crate) fn build_peer_client_cfg() -> Result<quinn::ClientConfig> {
    // No CA check: `connect_to_peer` pins the key to the dialed `NodeId` after the handshake.
    let mut tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Ed25519CertVerifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ProtoRole::Peer.alpn().into()];

    let quic = QuicClientConfig::try_from(tls)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(quic));

    cfg.transport_config(Arc::new(default_client_transport()));

    Ok(cfg)
}
