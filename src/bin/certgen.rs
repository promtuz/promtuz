//! Build using
//! ```
//! cargo build --release --bin certgen --all-features
//! ```

use std::{
    error::Error,
    fs,
    process::{self, Command},
};

use chacha20poly1305::aead::OsRng;
use common::quic::id::derive_node_id;

use p256::pkcs8::EncodePrivateKey;
use p256::{self, SecretKey, pkcs8::LineEnding};

static OUT_DIR: &str = "out";

/// will try to find CA.{KEY,PEM} in current directory
static CA: &str = "RootCA";

fn main() -> Result<(), Box<dyn Error>> {
    let out_name = std::env::args().nth(1);

    if !fs::exists(format!("{}.key", CA))? || !fs::exists(format!("{}.pem", CA))? {
        eprintln!("Move to directory with '{}.{{key,pem}}'", CA);
        process::exit(1);
    }

    // === generate new secp256r1 key ===
    let seckey = SecretKey::random(&mut OsRng);
    let pubkey = seckey.public_key();

    // === derive node ID from pubkey ===
    let id: common::quic::id::NodeId = derive_node_id(&pubkey);

    let out_name = out_name.unwrap_or(id.to_string());

    println!("Generated node ID: {}", id);

    let key_path = format!("{OUT_DIR}/{out_name}.key");
    let csr_path = format!("{OUT_DIR}/{out_name}.csr");
    let cert_path = format!("{OUT_DIR}/{out_name}.crt");

    // === write private key ===
    let pem = seckey
        .to_pkcs8_pem(LineEnding::LF)
        .expect("PEM encoding failed");
    fs::write(&key_path, pem).expect("write key failed");

    // === generate CSR (OpenSSL) ===
    // subject is /CN=<id>
    let csr_status = Command::new("openssl")
        .args([
            "req",
            "-new",
            "-key",
            &key_path,
            "-out",
            &csr_path,
            "-subj",
            &format!("/CN={id}"),
        ])
        .status()
        .expect("failed to run openssl req");

    if !csr_status.success() {
        panic!("openssl csr failed");
    }

    // === SAN extension file ===
    let ext = format!("subjectAltName = @alt_names\n\n[alt_names]\nDNS.1 = {id}\n");
    
    let ext_path = format!("{id}.ext");
    fs::write(&ext_path, &ext).unwrap();

    // === sign certificate with RootCA ===
    let cert_status = Command::new("openssl")
        .args(["x509", "-req",
            "-in", &csr_path,
            "-CA", &format!("{CA}.pem"),
            "-CAkey", &format!("{CA}.key"),
            "-CAcreateserial",
            "-out", &cert_path,
            "-days", "365",
            "-sha256",
            "-extfile", &ext_path,
        ])
        .status()
        .expect("failed to run openssl x509");

    if !cert_status.success() {
        panic!("openssl cert signing failed");
    }

    fs::remove_file(ext_path).ok();
    fs::remove_file(csr_path).ok();

    println!("Generated:");
    println!("  {}", key_path);
    println!("  {}", cert_path);
    println!("Signed by RootCA.pem / RootCA.key");

    Ok(())
}
