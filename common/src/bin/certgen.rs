//! Build using
//! ```
//! cargo build --release --bin certgen --all-features
//! ```

use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process;

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use common::node::capability::CAPABILITY_OID;
use common::node::capability::NodeCapabilities;
use common::quic::id::NodeId;
use rcgen::CertificateParams;
use rcgen::CustomExtension;
use rcgen::DnType;
use rcgen::Issuer;
use rcgen::KeyPair;
use rcgen::SanType;

static OUT_DIR: &str = "out";

/// will try to find CA.{KEY,PEM} in current directory
static CA: &str = "RootCA";

/// A CA-attestable capability, mapped to its [`NodeCapabilities`] bit. The CA
/// operator asserts these at sign time — a node can never self-assert one.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Capability {
    Relay,
    PushGateway,
    BlobStore,
    CallRelay,
    HighAvailability,
}

impl Capability {
    fn bit(self) -> u32 {
        match self {
            Capability::Relay => NodeCapabilities::RELAY,
            Capability::PushGateway => NodeCapabilities::PUSH_GATEWAY,
            Capability::BlobStore => NodeCapabilities::BLOB_STORE,
            Capability::CallRelay => NodeCapabilities::CALL_RELAY,
            Capability::HighAvailability => NodeCapabilities::HIGH_AVAILABILITY,
        }
    }
}

fn fold_caps(caps: &[Capability]) -> NodeCapabilities {
    caps.iter().fold(NodeCapabilities::empty(), |acc, c| acc.with(c.bit()))
}

/// The CA-signed capability extension for a cert (empty caps → no extension;
/// the caller guards on that).
fn capability_extension(caps: NodeCapabilities) -> CustomExtension {
    CustomExtension::from_oid_content(CAPABILITY_OID, caps.encode())
}

#[derive(Parser)]
#[command(name = "certgen")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate a new leaf certificate signed by the local CA.
    Gen {
        /// Output file name; defaults to the NodeId derived from the generated key.
        name: Option<String>,
        /// Capability bit to bake into the cert (repeatable), e.g.
        /// `--cap push-gateway`.
        #[arg(long = "cap", value_enum)]
        caps: Vec<Capability>,
    },
    /// Sign a CSR with the local CA.
    /// Omit the path to read PEM from stdin and print the signed cert to stdout.
    Sign {
        /// Path to PEM-encoded CSR; omit to use stdin/stdout.
        csr_path: Option<PathBuf>,
        /// Capability bit to bake into the cert (repeatable), e.g.
        /// `--cap push-gateway`.
        #[arg(long = "cap", value_enum)]
        caps: Vec<Capability>,
    },
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    let ca_secret_key = format!("{}.key", CA);
    let ca_certificate = format!("{}.pem", CA);

    if !fs::exists(&ca_secret_key)? || !fs::exists(&ca_certificate)? {
        eprintln!(
            "Move to directory with '{CA}.{{key,pem}}', current : {:?}",
            std::env::current_dir()?
        );
        process::exit(1);
    }

    let root_ca_secret = fs::read_to_string(&ca_secret_key)?;
    let root_ca_cert = fs::read_to_string(&ca_certificate)?;

    let root_ca = KeyPair::from_pkcs8_pem_and_sign_algo(&root_ca_secret, &rcgen::PKCS_ED25519)?;
    let issuer = Issuer::from_ca_cert_pem(&root_ca_cert, &root_ca).inspect_err(|e| {
        dbg!(e);
    })?;

    match cli.command {
        Command::Sign { csr_path, caps } => {
            let to_stdout = csr_path.is_none();

            let csr_pem = match &csr_path {
                Some(path) => fs::read_to_string(path)?,
                None => {
                    eprintln!("Paste CSR (Ctrl+D when done):");
                    eprintln!("- - - - - - - - - - - - - - - - - -");
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };

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
            let id = NodeId::new(pubkey);

            csr.params.distinguished_name = rcgen::DistinguishedName::new();
            csr.params.distinguished_name.push(DnType::CommonName, id.to_string());
            csr.params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

            let caps = fold_caps(&caps);
            if caps != NodeCapabilities::empty() {
                csr.params.custom_extensions.push(capability_extension(caps));
            }

            let cert = csr.signed_by(&issuer)?;

            if to_stdout {
                eprintln!("\n- - - - - - - - - - - - - - - - - -");
                eprintln!("Signed certificate:");
                eprintln!("- - - - - - - - - - - - - - - - - -");
                print!("{}", cert.pem());
            } else {
                fs::create_dir_all(OUT_DIR)?;
                fs::write(format!("{OUT_DIR}/{id}.crt"), cert.pem())?;
                println!("signed {id}.crt");
            }
        }

        Command::Gen { name, caps } => {
            let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;

            let id = NodeId::new(leaf_key.public_key_raw());

            let out_name = name.unwrap_or(id.to_string());

            let mut params = CertificateParams::default();
            params.distinguished_name.push(DnType::CommonName, id.to_string());
            params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

            let caps = fold_caps(&caps);
            if caps != NodeCapabilities::empty() {
                params.custom_extensions.push(capability_extension(caps));
            }

            let cert = params.signed_by(&leaf_key, &issuer)?;

            let leaf_cert_pem = cert.pem();
            let leaf_key_pem = leaf_key.serialize_pem();

            let key_path = format!("{OUT_DIR}/{out_name}.key");
            // let csr_path = format!("{OUT_DIR}/{out_name}.csr");
            let cert_path = format!("{OUT_DIR}/{out_name}.crt");

            fs::create_dir_all(OUT_DIR)?;
            fs::write(cert_path, leaf_cert_pem).unwrap();
            fs::write(key_path, leaf_key_pem).unwrap();
        }
    }

    Ok(())
}