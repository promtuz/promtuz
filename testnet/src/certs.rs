//! Ephemeral PKI for one sandbox run: a self-signed root CA plus per-node
//! leaf certs chained to it. Each leaf's Ed25519 key doubles as the node's
//! TLS key *and* its long-term identity key — the relay/resolver derive
//! their IPK/NodeId straight from `key_path` — exactly as
//! `common/src/bin/certgen.rs` does for production certs.

use std::net::Ipv4Addr;

use anyhow::Context;
use anyhow::Result;
use common::quic::id::NodeId;
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::DnType;
use rcgen::IsCa;
use rcgen::Issuer;
use rcgen::KeyPair;
use rcgen::KeyUsagePurpose;
use rcgen::PKCS_ED25519;
use rcgen::SanType;

/// The sandbox root CA. Holds the signing key so it can mint leaf certs on
/// demand; `cert_pem` is what every node points `root_ca_path` at.
pub struct Ca {
    cert_pem: String,
    key: KeyPair,
}

/// A freshly minted node leaf.
pub struct Leaf {
    pub cert_pem: String,
    pub key_pem: String,
    /// Raw Ed25519 public key as hex — what a relay/resolver seed's `key =`
    /// field deserializes from (`common::quic::id::NodeKey`).
    pub pubkey_hex: String,
    /// `BLAKE3(pubkey)` in base32 — the node's DHT id and its cert CN/SAN.
    pub node_id: NodeId,
}

impl Ca {
    pub fn new() -> Result<Self> {
        let key = KeyPair::generate_for(&PKCS_ED25519).context("generate CA key")?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.distinguished_name.push(DnType::CommonName, "promtuz-e2e-ca");
        let cert = params.self_signed(&key).context("self-sign CA")?;
        Ok(Self { cert_pem: cert.pem(), key })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Mint a CA-signed leaf. CN/SAN carry the base32 `NodeId` so a dialer
    /// that uses the NodeId as the TLS server name validates cleanly;
    /// `localhost` + `127.0.0.1` SANs are added as belt-and-braces.
    pub fn issue(&self) -> Result<Leaf> {
        let issuer = Issuer::from_ca_cert_pem(&self.cert_pem, &self.key).context("load CA issuer")?;

        let key = KeyPair::generate_for(&PKCS_ED25519).context("generate leaf key")?;
        let pubkey = key.public_key_raw();
        let node_id = NodeId::new(pubkey);
        let cn = node_id.to_string();

        let mut params = CertificateParams::default();
        params.distinguished_name.push(DnType::CommonName, cn.clone());
        params.subject_alt_names =
            vec![dns(&cn), dns("localhost"), SanType::IpAddress(Ipv4Addr::LOCALHOST.into())];
        let cert = params.signed_by(&key, &issuer).context("sign leaf")?;

        Ok(Leaf {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            pubkey_hex: hex::encode(pubkey),
            node_id,
        })
    }
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.to_string().try_into().expect("valid IA5 DNS name"))
}
