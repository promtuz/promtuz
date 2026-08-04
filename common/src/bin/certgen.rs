//! Build using
//! ```
//! cargo build --release --bin certgen --all-features
//! ```

use std::error::Error;
use std::fs;
use std::io::Read;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process;

use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use common::node::capability::CAPABILITY_OID;
use common::node::capability::NodeCapabilities;
use common::quic::id::NodeId;
use rcgen::BasicConstraints;
use rcgen::CertificateParams;
use rcgen::CustomExtension;
use rcgen::DnType;
use rcgen::ExtendedKeyUsagePurpose;
use rcgen::IsCa;
use rcgen::KeyUsagePurpose;
use rcgen::Issuer;
use rcgen::KeyPair;
use rcgen::SanType;
use sha2::Digest;
use sha2::Sha256;
use time::Duration;
use time::OffsetDateTime;

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
    fn flag(self) -> NodeCapabilities {
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
    caps.iter().fold(NodeCapabilities::empty(), |acc, c| acc | c.flag())
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
    /// Mint a fresh self-signed root CA as `RootCA.{key,pem}` in the current
    /// directory — the trust anchor every other subcommand issues under.
    Init {
        /// Subject/issuer CN. Leave it at the default so the result is a
        /// drop-in for the `RootCA.pem` that libcore bakes in and the relay
        /// .deb ships as `/etc/promtuz/ca.pem`.
        #[arg(long, default_value = "Promtuz")]
        cn: String,
        /// Validity window in days, counted from now.
        #[arg(long, default_value_t = 3650)]
        days: i64,
    },
    /// Generate a new leaf certificate signed by the local CA.
    Gen {
        /// Output file name; defaults to the NodeId derived from the generated key.
        name: Option<String>,
        /// Capability bit to bake into the cert (repeatable), e.g.
        /// `--cap push-gateway`.
        #[arg(long = "cap", value_enum)]
        caps: Vec<Capability>,
        /// Leaf validity window in days, counted from now.
        #[arg(long, default_value_t = 3650)]
        days: i64,
    },
    /// Sign a CSR with the local CA.
    /// Omit the path to read PEM from stdin and print the signed cert to stdout.
    ///
    /// Only the CSR's public key is used. Every other field — subject, SANs,
    /// basicConstraints, key usages — is discarded and rebuilt here, so a
    /// requester cannot influence what the certificate authorises.
    Sign {
        /// Path to PEM-encoded CSR; omit to use stdin/stdout.
        csr_path: Option<PathBuf>,
        /// Capability bit to bake into the cert (repeatable), e.g.
        /// `--cap push-gateway`.
        #[arg(long = "cap", value_enum)]
        caps: Vec<Capability>,
        /// Leaf validity window in days, counted from now.
        #[arg(long, default_value_t = 3650)]
        days: i64,
    },
}

/// Mint the root of trust: a self-signed Ed25519 CA written as
/// `RootCA.{key,pem}` in the current directory.
///
/// Shaped to match the CA it replaces — `CN=Promtuz`, Ed25519, `CA:TRUE` with
/// an unconstrained path length (leaves are signed directly; there is no
/// intermediate tier), 10 years by default. That matters because the cert is
/// not just a file: libcore `include_bytes!`es it and the relay .deb ships it
/// as `/etc/promtuz/ca.pem`, so a shape change is an app rebuild and a
/// redeploy.
///
/// Refuses to touch an existing key. Overwriting a CA key is unrecoverable and
/// silently orphans every cert ever issued under it, so moving the old one
/// aside is left as a deliberate act by the operator.
fn init_ca(key_path: &str, cert_path: &str, cn: &str, days: i64) -> Result<(), Box<dyn Error>> {
    let key = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;

    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, cn);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // Self-referential AKI, as the CA it replaces carried. webpki builds paths
    // by issuer DN and signature, so this changes nothing at verification time
    // — it is here so a `openssl x509 -text` of old and new differ only in the
    // key, and no one has to wonder whether the shape drifted.
    params.use_authority_key_identifier_extension = true;
    params.not_before = OffsetDateTime::now_utc();
    params.not_after = params.not_before + Duration::days(days);

    let cert = params.self_signed(&key)?;

    // `create_new` + 0600 in one syscall. The exists() check the other
    // subcommands do would be a race here, and this is the one file in the
    // project where losing that race destroys the trust root — so let the
    // filesystem arbitrate, and never let the key exist world-readable, not
    // even for the instant before a chmod.
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut key_file = opts.open(key_path).map_err(|e| {
        format!(
            "could not create {key_path} in {:?}: {e}\n\
             A CA may already be here — move it aside before minting another.",
            std::env::current_dir().unwrap_or_default()
        )
    })?;
    key_file.write_all(key.serialize_pem().as_bytes())?;
    fs::write(cert_path, cert.pem())?;

    let fingerprint = Sha256::digest(cert.der())
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");

    println!("CA minted in {:?}", std::env::current_dir()?);
    println!("  {key_path}  (0600 — the secret)");
    println!("  {cert_path}  (public)");
    println!("  CN={cn}, valid {days} days");
    println!("  SHA-256 fingerprint: {fingerprint}");
    println!();
    println!("Back up {key_path} NOW — it cannot be regenerated, and without it");
    println!("no new relay, resolver or gateway can ever be enrolled.");
    println!("Then rebuild: libcore bakes {cert_path} in, and the relay .deb ships it.");

    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    let ca_secret_key = format!("{}.key", CA);
    let ca_certificate = format!("{}.pem", CA);

    // `init` mints the very pair the guard below insists on, so it runs ahead
    // of it — and never reaches the `match` further down.
    if let Command::Init { cn, days } = &cli.command {
        return init_ca(&ca_secret_key, &ca_certificate, cn, *days);
    }

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
        Command::Init { .. } => unreachable!("init returns before the CA is loaded"),

        Command::Sign { csr_path, caps, days } => {
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
            let csr = rcgen::CertificateSigningRequestParams::from_pem(&csr_pem)?;

            // Refuse a CSR that asks to be a CA, loudly, before anything else.
            //
            // Strictly this is belt-and-braces: nothing from `csr.params` reaches
            // the certificate any more (see below). But a CSR requesting CA:TRUE
            // is not a mistake — it is someone trying to become an issuer under
            // this root — and silently downgrading it to a leaf would hand that
            // person a working cert and leave no trace. Fail, name the file, keep
            // the evidence.
            if !matches!(csr.params.is_ca, IsCa::NoCa | IsCa::ExplicitNoCa) {
                // Written straight to stderr rather than returned: `main` prints
                // a `Box<dyn Error>` with `Debug`, which would escape the newlines
                // into one unreadable line. This is the message that has to stop
                // an operator mid-paste, so it gets to be legible.
                eprintln!("REFUSING TO SIGN — this CSR asks to become a certificate authority.");
                eprintln!();
                eprintln!("  It requests basicConstraints CA:TRUE. A node certificate is never a CA.");
                eprintln!("  Signing it would let the holder mint a certificate for ANY identity in");
                eprintln!("  the network — including a PUSH_GATEWAY cert — and nothing in Promtuz");
                eprintln!("  can revoke that.");
                eprintln!();
                match &csr_path {
                    Some(p) => eprintln!("  Keep {} and find out who sent it.", p.display()),
                    None => eprintln!("  Keep that CSR and find out who sent it."),
                }
                process::exit(1);
            }

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

            // Build the certificate from scratch. We deliberately do NOT sign
            // `csr.params`.
            //
            // `CertificateSigningRequestParams::from_pem` parses the requester's
            // extensions into those params — rcgen 0.14 maps basicConstraints into
            // `is_ca`, plus keyUsage and extendedKeyUsage. Signing them made every
            // one of those fields attacker-controlled: a CSR asking for CA:TRUE
            // was issued an unconstrained CA under this root, and webpki accepts a
            // chain through it, so the holder could mint a leaf for any NodeId —
            // including a PUSH_GATEWAY cert, the one capability the code checks.
            //
            // Allowlist, never inherit-and-patch: overwriting the fields we happen
            // to know about leaves every field rcgen learns to parse *next*
            // silently attacker-controlled. rcgen already notes that
            // `name_constraints` is "not yet handled" — that is the next one.
            // Only `public_key`, which carries a verified proof of possession, is
            // taken from the request.
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::ExplicitNoCa;
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            // rcgen's default window is 1975..4096, which is not a validity period
            // so much as its absence. Bound it. The default is generous because
            // nothing in the tree renews a cert — the relay reads it once at
            // startup — so a short window would strand deployments; shorten this
            // once enrollment can re-run unattended.
            params.not_before = OffsetDateTime::now_utc();
            params.not_after = params.not_before + Duration::days(days);
            params.distinguished_name = rcgen::DistinguishedName::new();
            params.distinguished_name.push(DnType::CommonName, id.to_string());
            params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

            let caps = fold_caps(&caps);
            if !caps.is_empty() {
                params.custom_extensions.push(capability_extension(caps));
            }

            let cert = params.signed_by(&csr.public_key, &issuer)?;

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

        Command::Gen { name, caps, days } => {
            let leaf_key = KeyPair::generate_for(&rcgen::PKCS_ED25519)?;

            let id = NodeId::new(leaf_key.public_key_raw());

            let out_name = name.unwrap_or(id.to_string());

            // Same explicit shape as the `sign` path — there is no CSR here, so
            // nothing is attacker-influenced, but a leaf minted by `gen` and one
            // minted by `sign` should not differ in what they authorise.
            let mut params = CertificateParams::default();
            params.is_ca = IsCa::ExplicitNoCa;
            params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            params.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            params.not_before = OffsetDateTime::now_utc();
            params.not_after = params.not_before + Duration::days(days);
            params.distinguished_name.push(DnType::CommonName, id.to_string());
            params.subject_alt_names = vec![SanType::DnsName(id.to_string().try_into()?)];

            let caps = fold_caps(&caps);
            if !caps.is_empty() {
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