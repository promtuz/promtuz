//! Node enrollment: load or create the node's Ed25519 key, validate its CA-issued cert, or emit a
//! CSR and wait.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use base64::Engine as _;
use ed25519_dalek::Signer;
use ed25519_dalek::SigningKey;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::pki_types::UnixTime;

use crate::node::capability::NodeCapabilities;
use crate::quic::config::load_root_ca;
use crate::quic::id::NodeId;

fn read_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(n)?;
        (bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), rest)
    };
    let (value, rest) = rest.split_at_checked(len)?;
    Some((tag, value, rest))
}

/// The TBSCertificate from its SPKI on. Walking the fields instead of searching for a byte pattern
/// means a decoy elsewhere in the cert cannot be mistaken for a field.
fn tbs_from_spki(cert_der: &[u8]) -> Option<&[u8]> {
    let (0x30, cert, _) = read_tlv(cert_der)? else { return None };
    let (0x30, tbs, _) = read_tlv(cert)? else { return None };

    let (tag, _, after_version) = read_tlv(tbs)?;
    let mut rest = if tag == 0xa0 { after_version } else { tbs };
    // serialNumber, signature, issuer, validity, subject
    for _ in 0..5 {
        rest = read_tlv(rest)?.2;
    }
    Some(rest)
}

pub fn spki_ed25519(cert_der: &[u8]) -> Option<[u8; 32]> {
    let (0x30, spki, _) = read_tlv(tbs_from_spki(cert_der)?)? else { return None };
    let (0x30, algorithm, key) = read_tlv(spki)? else { return None };
    if algorithm != ED25519_AID {
        return None;
    }
    let (0x03, bits, _) = read_tlv(key)? else { return None };
    let [0x00, pubkey @ ..] = bits else { return None };
    pubkey.try_into().ok()
}

/// The DER value octets of [`crate::node::capability::CAPABILITY_OID`].
const CAPABILITY_OID_DER: &[u8] = &[0x2b, 0x06, 0x01, 0x04, 0x01, 0x83, 0xcc, 0x08, 0x01];

/// The CA-stamped capabilities extension. Trust it only from a cert whose chain verified.
pub fn cert_capabilities(cert_der: &[u8]) -> Option<NodeCapabilities> {
    // The unique ids [1] and [2] may sit between the SPKI and the extensions [3].
    let mut rest = read_tlv(tbs_from_spki(cert_der)?)?.2;
    let extensions = loop {
        let (tag, value, next) = read_tlv(rest)?;
        if tag == 0xa3 {
            break value;
        }
        rest = next;
    };
    let (0x30, mut extensions, _) = read_tlv(extensions)? else { return None };
    while !extensions.is_empty() {
        let (0x30, extension, next) = read_tlv(extensions)? else { return None };
        extensions = next;
        let (0x06, CAPABILITY_OID_DER, mut fields) = read_tlv(extension)? else { continue };
        // A `critical` BOOLEAN may precede the OCTET STRING.
        if fields.first() == Some(&0x01) {
            fields = read_tlv(fields)?.2;
        }
        let (0x04, value, _) = read_tlv(fields)? else { return None };
        return NodeCapabilities::decode(value);
    }
    None
}

pub fn first_cert_der(cert_path: &Path) -> anyhow::Result<CertificateDer<'static>> {
    let pem =
        std::fs::read(cert_path).with_context(|| format!("failed to read file '{cert_path:?}'"))?;
    let mut rd = std::io::BufReader::new(&pem[..]);
    rustls_pemfile::certs(&mut rd).flatten().next().with_context(|| {
        format!("failed to extract any valid certificate from file '{cert_path:?}'")
    })
}

/// Needs the process crypto provider installed first (`setup_crypto_provider`).
pub fn cert_is_valid(
    cert_path: &Path, ca_path: &Path, node_id: &NodeId, key_pub: &[u8; 32],
) -> anyhow::Result<bool> {
    if !cert_path.try_exists().with_context(|| format!("failed to check file '{cert_path:?}'"))? {
        bail!("certificate missing")
    }
    let leaf = first_cert_der(cert_path)?;
    verify_leaf(&leaf, ca_path, node_id, key_pub)?;
    Ok(true)
}

pub fn validate_cert_pem(
    pem: &[u8], ca_path: &Path, node_id: &NodeId, key_pub: &[u8; 32],
) -> anyhow::Result<()> {
    let mut rd = std::io::BufReader::new(pem);
    let leaf = rustls_pemfile::certs(&mut rd)
        .flatten()
        .next()
        .context("no certificate found in pasted input")?;
    verify_leaf(&leaf, ca_path, node_id, key_pub)
}

/// The chain to the root CA, the validity window, `node_id` as the name and `key_pub` as the key.
pub fn verify_leaf(
    leaf: &[u8], ca_path: &Path, node_id: &NodeId, key_pub: &[u8; 32],
) -> anyhow::Result<()> {
    if spki_ed25519(leaf).as_ref() != Some(key_pub) {
        bail!("provided certificate does not certify own key");
    }
    let roots = load_root_ca(&ca_path.to_path_buf()).context("failed to load root ca")?;
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .with_context(|| "failed to forge verified with root ca")?;
    let server_name = ServerName::try_from(node_id.to_string())
        .with_context(|| "failed to forge server name from node id")?;
    verifier
        .verify_server_cert(&CertificateDer::from(leaf), &[], &server_name, &[], UnixTime::now())
        .map(|_| ())
        .map_err(|e| anyhow!(e).context("webpki server verifier failed"))
}

const ED25519_AID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

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
    [&[0x30, 0x2a][..], &[0x30, 0x05][..], ED25519_AID, &[0x03, 0x21, 0x00][..], pubkey].concat()
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

pub fn csr_pem(signing: &SigningKey, node_id: &NodeId) -> String {
    let pubkey = signing.verifying_key().to_bytes();
    let info = csr_info(&pubkey, &node_id.to_string());
    let sig = signing.sign(&info).to_bytes();
    pem_wrap("CERTIFICATE REQUEST", &csr_der(&info, &sig))
}

pub fn emit_csr(csr_path: &Path, signing: &SigningKey, node_id: &NodeId) -> std::io::Result<()> {
    std::fs::write(csr_path, csr_pem(signing, node_id))
}

#[cfg(all(feature = "server", feature = "tokio"))]
pub use orchestrate::ensure_enrolled;
#[cfg(all(feature = "server", feature = "tokio"))]
pub use orchestrate::interactive;
#[cfg(all(feature = "server", feature = "tokio"))]
pub use orchestrate::spawn_config_reload;

#[cfg(all(feature = "server", feature = "tokio"))]
mod orchestrate {
    use std::ffi::OsStr;
    use std::io::Read as _;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    use anyhow::Context as _;
    use anyhow::anyhow;
    use anyhow::bail;
    use ed25519_dalek::SigningKey;
    use notify::RecursiveMode;
    use notify::Watcher as _;
    use serde::de::DeserializeOwned;

    use super::cert_is_valid;
    use super::csr_pem;
    use super::emit_csr;
    use super::validate_cert_pem;
    use crate::node::config::NetworkConfig;
    use crate::quic::config::setup_crypto_provider;
    use crate::quic::id::NodeId;
    use crate::quic::p256::secret_from_key;
    use crate::quic::p256::secret_from_key_or_create;

    /// Returns the node key once the cert validates; until then it waits on a CSR instead of
    /// crash-looping.
    pub async fn ensure_enrolled(
        net: &NetworkConfig, csr_path: &Path, role: &str,
    ) -> anyhow::Result<SigningKey> {
        setup_crypto_provider()?;

        let signing = secret_from_key_or_create(&net.key_path).map_err(|_| {
            anyhow::anyhow!("loading/creating the node key at {}", net.key_path.display())
        })?;
        let key_pub = signing.verifying_key().to_bytes();
        let node_id = NodeId::new(key_pub);

        if let Err(err) = cert_is_valid(&net.cert_path, &net.root_ca_path, &node_id, &key_pub) {
            crate::warn!("invalid certificate: {err}");
        } else {
            let _ = std::fs::remove_file(csr_path);
            return Ok(signing);
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
        let mut watcher = notify::recommended_watcher(move |_evt| {
            let _ = tx.blocking_send(());
        })?;
        watcher.watch(dir_of(&net.cert_path), RecursiveMode::NonRecursive)?;

        loop {
            tokio::select! {
                _ = rx.recv() => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            if cert_is_valid(&net.cert_path, &net.root_ca_path, &node_id, &key_pub).is_ok() {
                let _ = std::fs::remove_file(csr_path);
                crate::info!("{role} enrolled; cert accepted at {}", net.cert_path.display());
                return Ok(signing);
            }
        }
    }

    /// A bare file name's parent is empty, which notify cannot watch.
    fn dir_of(path: &Path) -> &Path {
        match path.parent() {
            Some(dir) if dir.as_os_str().is_empty() => Path::new("."),
            dir => dir.unwrap_or(Path::new("/etc/promtuz")),
        }
    }

    /// Prints the CSR, then installs a signed cert pasted on stdin.
    pub fn interactive(net: &NetworkConfig) -> anyhow::Result<()> {
        let _ = setup_crypto_provider();

        // Load only: a key minted here would be root-owned and unreadable by the service user.
        let signing = secret_from_key(&net.key_path).map_err(|_| {
            let path = net.key_path.display();
            anyhow!("no node key at {path}; start the daemon once to generate it")
        })?;
        let key_pub = signing.verifying_key().to_bytes();
        let node_id = NodeId::new(key_pub);

        if cert_is_valid(&net.cert_path, &net.root_ca_path, &node_id, &key_pub).unwrap_or(false) {
            println!("already enrolled: {} certifies node {node_id}", net.cert_path.display());
            return Ok(());
        }

        println!("{}", csr_pem(&signing, &node_id));
        eprintln!("↑ CSR for node {node_id}");
        eprintln!("Sign it (certgen sign), paste the signed cert below, then Ctrl-D:");

        let mut pem = String::new();
        std::io::stdin().read_to_string(&mut pem).context("reading cert from stdin")?;
        if pem.trim().is_empty() {
            bail!("no cert pasted");
        }

        validate_cert_pem(pem.as_bytes(), &net.root_ca_path, &node_id, &key_pub)
            .context("pasted cert rejected")?;
        std::fs::write(&net.cert_path, &pem)
            .with_context(|| format!("writing {}", net.cert_path.display()))?;

        println!(
            "enrolled: wrote {}. A running daemon starts serving automatically.",
            net.cert_path.display()
        );
        Ok(())
    }

    /// Only a write to the config file itself: the daemon opens other files in its directory.
    fn is_config_write(event: &notify::Event, name: Option<&OsStr>) -> bool {
        matches!(event.kind, notify::EventKind::Modify(_) | notify::EventKind::Create(_))
            && event.paths.iter().any(|path| path.file_name() == name)
    }

    #[derive(Debug, PartialEq)]
    enum Reload {
        Unchanged,
        Restart,
        Invalid,
    }

    /// A config that does not parse as `T` would stop the restarted daemon, so it is not applied.
    fn reload<T: DeserializeOwned>(current: &[u8], new: &[u8]) -> Reload {
        if new == current {
            return Reload::Unchanged;
        }
        match std::str::from_utf8(new).ok().and_then(|s| toml::from_str::<T>(s).ok()) {
            Some(_) => Reload::Restart,
            None => Reload::Invalid,
        }
    }

    /// Re-execs in place (same PID, no reliance on systemd `Restart=`) when the config file's
    /// bytes change and still parse as `T`. A parse failure keeps the current config.
    pub fn spawn_config_reload<T: DeserializeOwned>(config_path: PathBuf) {
        tokio::spawn(async move {
            let name = config_path.file_name().map(|n| n.to_os_string());
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(8);
            let Ok(mut watcher) =
                notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
                    if e.is_ok_and(|e| is_config_write(&e, name.as_deref())) {
                        let _ = tx.blocking_send(());
                    }
                })
            else {
                return;
            };
            if watcher.watch(dir_of(&config_path), RecursiveMode::NonRecursive).is_err() {
                return;
            }
            let _keep = watcher;
            let mut current = std::fs::read(&config_path).unwrap_or_default();

            while rx.recv().await.is_some() {
                // Debounce: editors emit several events per save.
                tokio::time::sleep(Duration::from_millis(300)).await;
                while rx.try_recv().is_ok() {}

                let Ok(bytes) = std::fs::read(&config_path) else { continue };
                let decision = reload::<T>(&current, &bytes);
                current = bytes;
                match decision {
                    Reload::Unchanged => {},
                    Reload::Restart => {
                        crate::info!("config changed and parses; restarting in place");
                        use std::os::unix::process::CommandExt as _;
                        let err =
                            std::process::Command::new(std::env::current_exe().unwrap_or_default())
                                .args(std::env::args_os().skip(1))
                                .exec();
                        crate::warn!("re-exec failed: {err}; staying on the old config");
                    },
                    Reload::Invalid => {
                        crate::warn!("config changed but failed to parse; keeping current config")
                    },
                }
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use std::collections::HashMap;

        use notify::Event;
        use notify::EventKind;
        use notify::event::AccessKind;
        use notify::event::AccessMode;
        use notify::event::CreateKind;
        use notify::event::DataChange;
        use notify::event::ModifyKind;

        use super::*;

        /// Restarting on every open of a file next to the config loops the daemon, and restarting
        /// into a config it cannot parse stops it.
        #[test]
        fn only_a_parsable_change_to_the_config_file_restarts() {
            let event = |kind, file: &str| Event::new(kind).add_path(file.into());
            let open = EventKind::Access(AccessKind::Open(AccessMode::Read));
            let write = EventKind::Modify(ModifyKind::Data(DataChange::Content));
            let create = EventKind::Create(CreateKind::File);
            let name = Some(OsStr::new("relay.toml"));
            for (label, sent, expected) in [
                ("an open of the config", event(open, "relay.toml"), false),
                ("a write to the CA beside it", event(write, "ca.pem"), false),
                ("a write to the config", event(write, "relay.toml"), true),
                ("a new config renamed into place", event(create, "relay.toml"), true),
            ] {
                assert_eq!(is_config_write(&sent, name), expected, "{label}");
            }

            type Config = HashMap<String, u16>;
            for (label, new, expected) in [
                ("the same bytes", &b"port = 1"[..], Reload::Unchanged),
                ("a new port", b"port = 2", Reload::Restart),
                ("valid TOML of the wrong type", b"port = \"two\"", Reload::Invalid),
                ("broken TOML", b"port =", Reload::Invalid),
                ("not UTF-8", b"\xff", Reload::Invalid),
            ] {
                assert_eq!(reload::<Config>(b"port = 1", new), expected, "{label}");
            }
        }
    }
}
