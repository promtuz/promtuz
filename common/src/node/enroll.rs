//! Node enrollment: load-or-create the single Ed25519 key, validate the
//! CA-issued cert, or emit a CSR and wait. Shared by relay and resolver.

use std::path::Path;
use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;

use base64::Engine as _;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;

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

// ---------------------------------------------------------------------------
// PKCS#10 CSR generation (hand-rolled Ed25519 DER; mirrors the self-signed
// cert DER in `quic::config`). `certgen sign` re-derives CN/SAN from the
// pubkey, so the request carries only version + subject(CN) + SPKI + empty
// attributes, self-signed (proof of possession).
// ---------------------------------------------------------------------------

const ED25519_AID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

/// Minimal DER tag-length-value with short/long-form length.
fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let n = body.len();
    if n < 128 {
        [&[tag, n as u8][..], body].concat()
    } else if n < 256 {
        [&[tag, 0x81, n as u8][..], body].concat()
    } else {
        [&[tag, 0x82][..], &(n as u16).to_be_bytes(), body].concat()
    }
}

fn spki_der(pubkey: &[u8; 32]) -> Vec<u8> {
    [
        &[0x30, 0x2a][..],
        &[0x30, 0x05][..],
        ED25519_AID,
        &[0x03, 0x21, 0x00][..],
        pubkey,
    ]
    .concat()
}

/// CertificationRequestInfo: version(0) + subject(CN) + SPKI + empty attrs.
fn csr_info(pubkey: &[u8; 32], cn: &str) -> Vec<u8> {
    let version: &[u8] = &[0x02, 0x01, 0x00];
    let cn_oid: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03]; // id-at-commonName
    let cn_utf8 = [&[0x0c, cn.len() as u8][..], cn.as_bytes()].concat();
    // Name ::= SEQUENCE OF RDN; RDN ::= SET OF AttributeTypeAndValue
    let subject = tlv(0x30, &tlv(0x31, &tlv(0x30, &[cn_oid, &cn_utf8].concat())));
    let attributes: &[u8] = &[0xa0, 0x00]; // [0] IMPLICIT SET OF, empty
    tlv(0x30, &[version, &subject, &spki_der(pubkey), attributes].concat())
}

fn csr_der(info: &[u8], sig: &[u8; 64]) -> Vec<u8> {
    let sig_alg = tlv(0x30, ED25519_AID);
    let sig_bits = [&[0x03, 0x41, 0x00][..], sig].concat();
    tlv(0x30, &[info, &sig_alg, &sig_bits].concat())
}

fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

/// Write a PKCS#10 CSR (PEM) for `signing`'s key, `CN = base32(node_id)`.
pub fn emit_csr(
    csr_path: &Path, signing: &SigningKey, node_id: &NodeId,
) -> std::io::Result<()> {
    let pubkey = signing.verifying_key().to_bytes();
    let info = csr_info(&pubkey, &node_id.to_string());
    let sig = signing.sign(&info).to_bytes();
    let der = csr_der(&info, &sig);
    std::fs::write(csr_path, pem_wrap("CERTIFICATE REQUEST", &der))
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

    #[cfg(feature = "certgen")]
    #[test]
    fn csr_parses_with_rcgen() {
        let key = SigningKey::from_bytes(&[3u8; 32]);
        let id = NodeId::new(&key.verifying_key().to_bytes());
        let path = std::env::temp_dir().join("pz_csr_roundtrip.csr");
        emit_csr(&path, &key, &id).unwrap();
        let pem = std::fs::read_to_string(&path).unwrap();
        // from_pem validates the PKCS#10 structure AND the self-signature.
        rcgen::CertificateSigningRequestParams::from_pem(&pem)
            .expect("rcgen parses our hand-rolled CSR");
        let _ = std::fs::remove_file(&path);
    }

    // Full loop: ephemeral CA → emit_csr → sign exactly as `certgen sign` →
    // cert_is_valid must accept. Guards the keystone (a wrong reject = a node
    // that waits for enrollment forever).
    #[cfg(feature = "certgen")]
    #[test]
    fn accepts_our_ca_signed_cert() {
        use rcgen::BasicConstraints;
        use rcgen::CertificateParams;
        use rcgen::DnType;
        use rcgen::IsCa;
        use rcgen::Issuer;
        use rcgen::KeyPair;
        use rcgen::SanType;

        let _ = crate::quic::config::setup_crypto_provider();

        // Ephemeral CA.
        let ca_key = KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let ca_pem = ca_cert.pem();

        // Node key + CSR.
        let node = SigningKey::from_bytes(&[9u8; 32]);
        let id = NodeId::new(&node.verifying_key().to_bytes());
        let dir = std::env::temp_dir();
        let csr_path = dir.join("pz_pos.csr");
        emit_csr(&csr_path, &node, &id).unwrap();

        // Sign exactly as `certgen sign` does (CN/SAN derived from the key).
        let csr_pem = std::fs::read_to_string(&csr_path).unwrap();
        let mut csr = rcgen::CertificateSigningRequestParams::from_pem(&csr_pem).unwrap();
        csr.params.distinguished_name = rcgen::DistinguishedName::new();
        csr.params.distinguished_name.push(DnType::CommonName, id.to_string());
        csr.params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into().unwrap())];
        let issuer = Issuer::from_ca_cert_pem(&ca_pem, &ca_key).unwrap();
        let cert = csr.signed_by(&issuer).unwrap();

        let cert_path = dir.join("pz_pos.crt");
        let ca_path = dir.join("pz_pos_ca.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&ca_path, &ca_pem).unwrap();

        assert!(cert_is_valid(&cert_path, &ca_path, &id, &node.verifying_key().to_bytes()));

        for p in [csr_path, cert_path, ca_path] {
            let _ = std::fs::remove_file(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Async enrollment orchestration (daemon-side: needs tokio + notify).
// Kept in a gated submodule so `certgen` (quic+crypto, no tokio) can still
// use the sync helpers above.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "server", feature = "tokio"))]
pub use orchestrate::ensure_enrolled;
#[cfg(all(feature = "server", feature = "tokio"))]
pub use orchestrate::spawn_config_reload;

#[cfg(all(feature = "server", feature = "tokio"))]
mod orchestrate {
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    use notify::RecursiveMode;
    use notify::Watcher as _;

    use super::cert_is_valid;
    use super::emit_csr;
    use crate::node::config::NetworkConfig;
    use crate::quic::config::setup_crypto_provider;
    use crate::quic::id::NodeId;
    use crate::quic::p256::secret_from_key_or_create;

    /// Ensure the node holds a valid cert for its key, or write a CSR and wait.
    /// Returns once `cert_path` validates; otherwise blocks (never crash-loops).
    pub async fn ensure_enrolled(
        net: &NetworkConfig, csr_path: &Path, role: &str,
    ) -> anyhow::Result<()> {
        setup_crypto_provider()?;

        let signing = secret_from_key_or_create(&net.key_path).map_err(|_| {
            anyhow::anyhow!("loading/creating the node key at {}", net.key_path.display())
        })?;
        let key_pub = signing.verifying_key().to_bytes();
        let node_id = NodeId::new(&key_pub);

        if cert_is_valid(&net.cert_path, &net.root_ca_path, &node_id, &key_pub) {
            let _ = std::fs::remove_file(csr_path);
            return Ok(());
        }

        emit_csr(csr_path, &signing, &node_id)?;
        crate::warn!(
            "{role} not enrolled. Wrote CSR to {}. Sign it on the CA box \
             (`certgen sign {}`), drop the signed cert at {}, and I start automatically.",
            csr_path.display(),
            csr_path.display(),
            net.cert_path.display(),
        );

        // Watch the cert dir; a 5s poll backstops any missed inotify event.
        let watch_dir = net
            .cert_path
            .parent()
            .unwrap_or_else(|| Path::new("/etc/promtuz"))
            .to_path_buf();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
        let mut watcher = notify::recommended_watcher(move |_evt| {
            let _ = tx.blocking_send(());
        })?;
        watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;

        loop {
            tokio::select! {
                _ = rx.recv() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            if cert_is_valid(&net.cert_path, &net.root_ca_path, &node_id, &key_pub) {
                let _ = std::fs::remove_file(csr_path);
                crate::info!("{role} enrolled; cert accepted at {}", net.cert_path.display());
                return Ok(());
            }
        }
    }

    /// Watch the config file; on a change that still parses, re-exec the
    /// process in place (PID-stable, independent of the systemd `Restart=`
    /// policy). A parse failure logs and keeps running on the current config,
    /// so a bad edit never crash-loops the node. Opt-in via `watch_reload`.
    pub fn spawn_config_reload(config_path: PathBuf) {
        tokio::spawn(async move {
            let dir = config_path
                .parent()
                .unwrap_or_else(|| Path::new("/etc/promtuz"))
                .to_path_buf();
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
            let Ok(mut watcher) = notify::recommended_watcher(move |_e| {
                let _ = tx.blocking_send(());
            }) else {
                return;
            };
            if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            let _keep = watcher;

            while rx.recv().await.is_some() {
                // Debounce: editors emit several events per save.
                tokio::time::sleep(Duration::from_millis(300)).await;
                while rx.try_recv().is_ok() {}

                match std::fs::read_to_string(&config_path)
                    .ok()
                    .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
                {
                    Some(_) => {
                        crate::info!("config changed and parses; restarting in place");
                        use std::os::unix::process::CommandExt as _;
                        let err = std::process::Command::new(
                            std::env::current_exe().unwrap_or_default(),
                        )
                        .args(std::env::args_os().skip(1))
                        .exec();
                        crate::warn!("re-exec failed: {err}; staying on the old config");
                    },
                    None => crate::warn!(
                        "config changed but failed to parse; keeping current config"
                    ),
                }
            }
        });
    }
}
