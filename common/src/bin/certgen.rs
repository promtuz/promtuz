//! Build using
//! ```
//! cargo build --release --bin certgen --all-features
//! ```

use std::error::Error;
use std::fs;
use std::process::{
    self,
};
use common::quic::id::NodeId;
use rcgen::{CertificateParams, SanType};
use rcgen::DnType;
use rcgen::Issuer;
use rcgen::KeyPair;

static OUT_DIR: &str = "out";

/// will try to find CA.{KEY,PEM} in current directory
static CA: &str = "RootCA";

fn main() -> Result<(), Box<dyn Error>> {
    let out_name: Option<String> = std::env::args().nth(1);

    let ca_secret_key = format!("{}.key", CA);
    let ca_certificate = format!("{}.pem", CA);

    if !fs::exists(&ca_secret_key)? || !fs::exists(&ca_certificate)? {
        eprintln!("Move to directory with '{}.{{key,pem}}'", CA);
        process::exit(1);
    }

    let root_ca_secret = fs::read_to_string(&ca_secret_key)?;
    let root_ca_cert = fs::read_to_string(&ca_certificate)?;

    let root_ca = KeyPair::from_pkcs8_pem_and_sign_algo(&root_ca_secret, &rcgen::PKCS_ED25519)?;
    let issuer = Issuer::from_ca_cert_pem(&root_ca_cert, &root_ca).inspect_err(|e| {
        dbg!(e);
    })?;

    // `certgen sign <csr>`: issue a CA cert for an existing CSR's key.
    if std::env::args().nth(1).as_deref() == Some("sign") {
        let csr_path = std::env::args().nth(2).ok_or("usage: certgen sign <csr-path>")?;
        let csr_pem = fs::read_to_string(&csr_path)?;

        // rcgen parses the CSR and verifies its PKCS#10 self-signature (proof
        // of possession) against the key it exposes as `public_key`.
        use rcgen::PublicKeyData as _;
        let mut csr = rcgen::CertificateSigningRequestParams::from_pem(&csr_pem)?;

        // Derive identity ONLY from `public_key` — the exact key rcgen verified
        // PoP against, and the one `signed_by` embeds as the cert SPKI. Never
        // byte-scan the raw CSR: the attacker controls the subject/attribute
        // bytes and could smuggle a second SPKI past a scan, yielding a cert
        // whose real key is the attacker's but whose CN is a victim's NodeId.
        if csr.public_key.algorithm() != &rcgen::PKCS_ED25519 {
            return Err("CSR is not Ed25519".into());
        }
        let pubkey: [u8; 32] = csr
            .public_key
            .der_bytes()
            .try_into()
            .map_err(|_| "CSR public key is not a 32-byte Ed25519 key")?;
        let id = NodeId::new(&pubkey);

        csr.params.distinguished_name = rcgen::DistinguishedName::new();
        csr.params.distinguished_name.push(DnType::CommonName, id.to_string());
        csr.params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

        let cert = csr.signed_by(&issuer)?;
        fs::create_dir_all(OUT_DIR)?;
        fs::write(format!("{OUT_DIR}/{id}.crt"), cert.pem())?;
        println!("signed {id}.crt");
        return Ok(());
    }

    let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;

    let id = NodeId::new(leaf_key.public_key_raw());

    let out_name = out_name.unwrap_or(id.to_string());

    let mut params = CertificateParams::default();
    params.distinguished_name.push(DnType::CommonName, id.to_string());
    params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

    let cert = params.signed_by(&leaf_key, &issuer)?;

    let leaf_cert_pem = cert.pem();
    let leaf_key_pem = leaf_key.serialize_pem();

    let key_path = format!("{OUT_DIR}/{out_name}.key");
    // let csr_path = format!("{OUT_DIR}/{out_name}.csr");
    let cert_path = format!("{OUT_DIR}/{out_name}.crt");

    fs::write(cert_path, leaf_cert_pem).unwrap();
    fs::write(key_path, leaf_key_pem).unwrap();

    Ok(())
}
