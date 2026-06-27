#![cfg(not(doctest))]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use std::time::Duration;

use crate::quic::protorole::ProtoRole;
use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use quinn::IdleTimeout;
use quinn::ServerConfig as QuinnServerConfig;
use quinn::TransportConfig;
use quinn::VarInt;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::RootCertStore;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;
#[cfg(feature = "crypto")]
use rustls::server::ClientHello;
#[cfg(feature = "crypto")]
use rustls::server::ResolvesServerCert;
#[cfg(feature = "crypto")]
use rustls::sign::CertifiedKey;

/// Defaults applied to every server-side QUIC connection. Caps connection
/// lifetime and per-connection stream budget so one misbehaving peer cannot
/// consume unbounded resources.
///
/// Keepalive lives on the client (see `default_client_transport`); a server
/// pinging every idle Android client would multiply idle traffic and battery
/// cost without buying anything — the idle timeout already evicts dead peers.
fn default_server_transport() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is a valid IdleTimeout"),
    ));
    tc.max_concurrent_bidi_streams(VarInt::from_u32(64));
    tc.max_concurrent_uni_streams(VarInt::from_u32(64));
    tc
}

/// Outbound-connection defaults. Keepalive every 10 s refreshes the server's
/// 30 s idle timer so a legitimately quiet client (e.g. nothing to send) does
/// not get evicted.
fn default_client_transport() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is a valid IdleTimeout"),
    ));
    tc.keep_alive_interval(Some(Duration::from_secs(10)));
    tc.max_concurrent_bidi_streams(VarInt::from_u32(64));
    tc.max_concurrent_uni_streams(VarInt::from_u32(64));
    tc
}

pub fn setup_crypto_provider() -> Result<()> {
    if CryptoProvider::get_default().is_none() {
        CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
            .map_err(|_| anyhow!("installing the default crypto provider"))?;
    }
    Ok(())
}

pub fn load_root_ca_bytes(bytes: &[u8]) -> Result<rustls::RootCertStore> {
    let mut store = rustls::RootCertStore::empty();
    let mut reader = std::io::BufReader::new(bytes);

    let certs = rustls_pemfile::certs(&mut reader).flatten();
    let (certs_added, _) = store.add_parsable_certificates(certs);

    if certs_added == 0 {
        return Err(anyhow!("ERROR: could not add any root_ca"));
    }

    Ok(store)
}

pub fn load_root_ca(path: &PathBuf) -> Result<rustls::RootCertStore> {
    let bytes = std::fs::read(path)?;
    load_root_ca_bytes(&bytes)
}

/// Builds a QUIC server configuration using a TLS certificate, private key,
/// and a list of ALPN protocols the server is willing to accept.
///
/// This function loads the TLS material from disk, constructs a
/// `rustls::ServerConfig`, attaches the provided ALPN protocol list,
/// and converts it into a `quinn::ServerConfig` suitable for creating
/// a QUIC endpoint.
///
/// ## Parameters
///
/// * `cert_path`  
///   Filesystem path to a PEM-encoded X.509 certificate chain.
///
/// * `key_path`  
///   Filesystem path to a PEM-encoded private key corresponding to the certificate.
///
/// * `alpn_protocols`  
///   A static list of application protocols (ALPN) this server is
///   willing to negotiate.  
///   Only connections offering one of these protocols will be accepted.
///
/// ## Returns
///
/// Returns a fully initialized [`quinn::ServerConfig`] wrapped in an
/// application-specific `QuinnServerConfig` type (or as defined in your
/// codebase).  
/// This configuration can be passed to `Endpoint::server` to create a
/// listening QUIC endpoint.
///
/// ## Errors
///
/// Returns an error if:
/// - certificate or key files cannot be read or parsed
/// - TLS configuration cannot be constructed (e.g., invalid key format)
/// - ALPN configuration is invalid for the TLS backend
///
/// ## Example
///
/// ```no_run
/// let cfg = build_server_cfg(
///     Path::new("cert/server.crt"),
///     Path::new("cert/server.key"),
///     &["resolver/1", "node/1", "client/1"],
/// )?;
/// let endpoint = quinn::Endpoint::server(cfg, "0.0.0.0:4433".parse()?)?;
/// ```
///
/// ## Notes
///
/// * ALPN determines *what roles* this server is willing to accept, but
///   the **dialer** decides the actual role of a connection by choosing
///   the ALPN it offers during the handshake.
/// * Only inbound connections use this configuration. Outbound connections
///   must use a separate client configuration with a single ALPN.
pub fn build_server_cfg(
    cert_path: &Path,
    key_path: &Path,
    alpn_protocols: &'static [ProtoRole],
) -> Result<QuinnServerConfig> {
    let mut cert_reader: BufReader<File> = BufReader::new(
        File::open(cert_path).with_context(|| format!("reading TLS cert at {}", cert_path.display()))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(
        File::open(key_path).with_context(|| format!("reading TLS key at {}", key_path.display()))?,
    );

    let key = rustls_pemfile::private_key(&mut key_reader)?.ok_or(anyhow!("No Private Key"))?;

    let mut tls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    tls.alpn_protocols = alpn_protocols
        .iter()
        .map(|prot| prot.alpn().into())
        .collect::<Vec<Vec<u8>>>();

    let quic_crypto = QuicServerConfig::try_from(tls)?;
    let mut server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_crypto));
    server_cfg.transport_config(Arc::new(default_server_transport()));

    Ok(server_cfg)
}

/// Builds a `quinn::ClientConfig` configured for a specific ALPN protocol.
///
/// This function is used whenever an outbound QUIC connection is made
/// (resolver→resolver, node→resolver, node→node, client→resolver, etc).
///
/// ## ALPN and roles
/// The **ALPN string defines the role of this outbound connection**:
///
/// - `"relay/1"`     — a relay/node dialing a resolver  
/// - `"resolver/1"` — a resolver dialing another resolver  
/// - `"peer/1"`     — a relay/node dialing another relay/node  
/// - `"client/1"`   — a client dialing a resolver  
///
/// Each outbound QUIC connection must advertise **exactly one** ALPN.
/// The receiving side uses the negotiated ALPN to route the connection
/// to the correct handler.
///
/// ## Root certificates
/// The caller must supply a `RootCertStore` containing the trusted
/// certificate authorities (CA roots) for this client.
/// This is what enables TLS verification of the remote endpoint.
///
/// Typically you load this from:
/// - your custom root CA (`rootCA.pem`)  
/// - system roots  
/// - resolver-issued CA (future feature)  
///
/// ## Returns
/// A fully initialized `quinn::ClientConfig`, ready to be:
/// - passed to `Endpoint::set_default_client_config()`, or  
/// - used directly when dialing:  
///   `endpoint.connect(addr, "hostname")?.await?`
///
/// ## Example
/// ```ignore
/// let roots = load_root_ca("cert/rootCA.pem")?;
/// let cfg = build_client_cfg("node/1", &roots)?;
/// endpoint.set_default_client_config(cfg);
///
/// let conn = endpoint
///     .connect("1.2.3.4:4433".parse().unwrap(), "resolver-host")?
///     .await?;
/// ```
pub fn build_client_cfg(role: ProtoRole, roots: &RootCertStore) -> Result<quinn::ClientConfig> {
    // --- rustls TLS config ---
    let mut tls = rustls::ClientConfig::builder()
        // .with_safe_defaults()
        .with_root_certificates(roots.clone())
        .with_no_client_auth(); // no client certificate auth

    // Set ALPN (only one per outbound role)
    tls.alpn_protocols = vec![role.alpn().into()];

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;

    // Wrap TLS config for Quinn
    let mut client = quinn::ClientConfig::new(Arc::new(quic_config));

    client.transport_config(Arc::new(default_client_transport()));

    Ok(client)
}
#[allow(dead_code)]
fn _phase8_section_marker() {}

// ===========================================================================
// NodeKey-as-SPKI cert for the peer/1 ALPN
// ===========================================================================
//
// The relay's primary TLS cert is CA-issued (by the project's RootCA) so it
// can present a chain to clients/resolvers that already trust the root. But
// libcore's `Peer1DhtClient` pins the relay's cert SPKI against the relay's
// NodeKey pubkey (vended by the resolver via `RelayDescriptor.pubkey`); the
// CA-issued cert's SPKI is **unrelated** to the NodeKey, so pinning over the
// CA-issued cert could never fire.
//
// Approach: serve a *separate* self-signed Ed25519 cert on the peer/1 ALPN
// where SPKI = NodeKey. We attach an ALPN-discriminating
// `ResolvesServerCert` to the rustls server config; the peer/1 ALPN gets the
// NodeKey-bound cert, every other ALPN keeps the CA-issued cert.
//
// rustls 0.23's `ClientHello::alpn()` exposes the offered ALPN list during
// the resolver callback, so this is cleanly supported by quinn 0.11 (which
// just delegates the cert resolution to rustls).
//
// We hand-roll the Ed25519 self-signed DER (mirroring libcore's
// `peer_config.rs::build_tbs_certificate` / `build_certificate_der` —
// kept duplicated rather than cross-crate to avoid a feature-flag
// rats-nest; both copies are <100 LOC and stable RFC 8410 wire format).

#[cfg(feature = "crypto")]
const ED25519_AID_DER: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

/// Build a hand-rolled DER X.509 TBSCertificate carrying `public_key`
/// in the SPKI. Matches libcore's `peer_config::build_tbs_certificate`
/// byte-for-byte so post-handshake SPKI extraction is symmetric.
#[cfg(feature = "crypto")]
fn build_tbs_certificate_for(public_key: &[u8; 32]) -> Vec<u8> {
    let spki = [
        &[0x30, 0x2a][..],
        &[0x30, 0x05][..],
        ED25519_AID_DER,
        &[0x03, 0x21, 0x00][..],
        public_key,
    ]
    .concat();

    let serial = &public_key[0..8];

    let validity: &[u8] = &[
        0x30, 0x1e, 0x17, 0x0d,
        b'7', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z', 0x17,
        0x0d, b'5', b'0', b'0', b'1', b'0', b'1', b'0', b'0', b'0', b'0', b'0', b'0', b'Z',
    ];

    let empty_name: &[u8] = &[0x30, 0x00];
    let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];
    let serial_der = [&[0x02, serial.len() as u8][..], serial].concat();
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    let tbs_content = [
        version,
        &serial_der,
        sig_alg,
        empty_name,
        validity,
        empty_name,
        &spki,
    ]
    .concat();

    encode_seq(&tbs_content)
}

/// Wrap signed TBS + sig into the final X.509 Certificate DER.
#[cfg(feature = "crypto")]
fn build_certificate_der_for(tbs: &[u8], signature: &[u8; 64]) -> Vec<u8> {
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];
    let sig_bitstring = [&[0x03, 0x41, 0x00][..], signature].concat();
    let cert_content = [tbs, sig_alg, &sig_bitstring].concat();
    encode_seq(&cert_content)
}

#[cfg(feature = "crypto")]
fn encode_seq(data: &[u8]) -> Vec<u8> {
    let len = data.len();
    if len < 128 {
        [&[0x30, len as u8][..], data].concat()
    } else if len < 256 {
        [&[0x30, 0x81, len as u8][..], data].concat()
    } else {
        let len_bytes = (len as u16).to_be_bytes();
        [&[0x30, 0x82][..], &len_bytes, data].concat()
    }
}

/// rustls SigningKey impl backed by an Ed25519 SigningKey — used as the
/// `cert_resolver`'s key half for the NodeKey-bound peer/1 cert.
#[cfg(feature = "crypto")]
#[derive(Debug)]
struct Ed25519SigningKey {
    public_key: ed25519_dalek::VerifyingKey,
    signing:    Arc<ed25519_dalek::SigningKey>,
}

#[cfg(feature = "crypto")]
impl rustls::sign::SigningKey for Ed25519SigningKey {
    fn choose_scheme(
        &self, offered: &[rustls::SignatureScheme],
    ) -> Option<Box<dyn rustls::sign::Signer>> {
        if offered.contains(&rustls::SignatureScheme::ED25519) {
            Some(Box::new(Ed25519Signer { signing: Arc::clone(&self.signing) }))
        } else {
            None
        }
    }

    fn public_key(&self) -> Option<rustls::pki_types::SubjectPublicKeyInfoDer<'_>> {
        let alg_id = rustls::pki_types::AlgorithmIdentifier::from_slice(ED25519_AID_DER);
        Some(rustls::sign::public_key_to_spki(&alg_id, self.public_key.as_bytes()))
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ED25519
    }
}

#[cfg(feature = "crypto")]
#[derive(Debug)]
struct Ed25519Signer {
    signing: Arc<ed25519_dalek::SigningKey>,
}

#[cfg(feature = "crypto")]
impl rustls::sign::Signer for Ed25519Signer {
    fn sign(&self, message: &[u8]) -> std::result::Result<Vec<u8>, rustls::Error> {
        use ed25519_dalek::Signer;
        Ok(self.signing.sign(message).to_bytes().to_vec())
    }

    fn scheme(&self) -> rustls::SignatureScheme {
        rustls::SignatureScheme::ED25519
    }
}

/// Build a `CertifiedKey` carrying a self-signed Ed25519 cert whose SPKI
/// is `signing.verifying_key()` — used by the relay to serve a
/// NodeKey-bound cert on the peer/1 ALPN so libcore-side pinning can fire.
#[cfg(feature = "crypto")]
pub fn build_self_signed_ed25519_cert(
    signing: ed25519_dalek::SigningKey,
) -> CertifiedKey {
    use ed25519_dalek::Signer;
    let signing_arc = Arc::new(signing);
    let public_key = signing_arc.verifying_key();
    let pub_bytes = public_key.to_bytes();

    let tbs = build_tbs_certificate_for(&pub_bytes);
    let sig = signing_arc.sign(&tbs);
    let cert_der = build_certificate_der_for(&tbs, &sig.to_bytes());

    let certs = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let signing_key: Arc<dyn rustls::sign::SigningKey> = Arc::new(Ed25519SigningKey {
        public_key,
        signing: signing_arc,
    });

    CertifiedKey::new(certs, signing_key)
}

/// ALPN-aware `ResolvesServerCert` used by the relay's QUIC server config.
///
/// Holds two `CertifiedKey`s: one for the peer/1 ALPN (NodeKey-bound,
/// so libcore can pin SPKI against `RelayDescriptor.pubkey`) and one
/// for everything else (the CA-issued cert that resolver/relay/client
/// dialers expect). The split is necessary because libcore's pinning
/// path checks `presented_spki == claimed_node_key`, which the
/// CA-issued cert cannot satisfy.
#[cfg(feature = "crypto")]
#[derive(Debug)]
pub struct AlpnAwareCertResolver {
    /// Cert served when ClientHello carries the `peer/1` ALPN.
    pub peer_cert: Arc<CertifiedKey>,
    /// Cert served for every other ALPN (resolver/1, relay/1, client/1)
    /// — typically the CA-issued cert from the operator's PKI.
    pub default_cert: Arc<CertifiedKey>,
}

#[cfg(feature = "crypto")]
impl ResolvesServerCert for AlpnAwareCertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let peer_alpn = ProtoRole::Peer.alpn();
        let peer_alpn_bytes = peer_alpn.as_bytes();
        if let Some(alpns) = hello.alpn() {
            for alpn in alpns {
                if alpn == peer_alpn_bytes {
                    return Some(Arc::clone(&self.peer_cert));
                }
            }
        }
        Some(Arc::clone(&self.default_cert))
    }
}

/// Variant of [`build_server_cfg`] that wires an [`AlpnAwareCertResolver`]
/// so the peer/1 ALPN is served a NodeKey-bound self-signed Ed25519 cert
/// while every other ALPN is served the operator's CA-issued cert.
///
/// `node_signing` is the relay's long-term Ed25519 NodeKey (i.e. the same
/// key whose pubkey is derived as `relay_id`/`node_id` and that the
/// resolver vends in `RelayDescriptor.pubkey`).
#[cfg(feature = "crypto")]
pub fn build_server_cfg_with_alpn_split(
    cert_path: &Path,
    key_path: &Path,
    node_signing: ed25519_dalek::SigningKey,
    alpn_protocols: &'static [ProtoRole],
) -> Result<QuinnServerConfig> {
    let mut cert_reader = BufReader::new(
        File::open(cert_path).with_context(|| format!("reading TLS cert at {}", cert_path.display()))?,
    );
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(
        File::open(key_path).with_context(|| format!("reading TLS key at {}", key_path.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or(anyhow!("No Private Key"))?;

    // Wrap the CA-issued cert+key into a CertifiedKey via rustls's
    // "any_supported_type" key parser.
    let signing_key = rustls::crypto::CryptoProvider::get_default()
        .ok_or_else(|| anyhow!("crypto provider not installed"))?
        .key_provider
        .load_private_key(key)
        .map_err(|e| anyhow!("load default cert key: {e}"))?;
    let default_cert = Arc::new(CertifiedKey::new(certs, signing_key));

    let peer_cert = Arc::new(build_self_signed_ed25519_cert(node_signing));

    let resolver = Arc::new(AlpnAwareCertResolver { peer_cert, default_cert });

    let mut tls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);

    tls.alpn_protocols = alpn_protocols
        .iter()
        .map(|prot| prot.alpn().into())
        .collect::<Vec<Vec<u8>>>();

    let quic_crypto = QuicServerConfig::try_from(tls)?;
    let mut server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_crypto));
    server_cfg.transport_config(Arc::new(default_server_transport()));
    Ok(server_cfg)
}
