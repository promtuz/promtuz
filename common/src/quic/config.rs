#![cfg(not(doctest))]

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::quic::protorole::ProtoRole;
use anyhow::Result;
use anyhow::anyhow;
use quinn::ServerConfig as QuinnServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::RootCertStore;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;

pub fn setup_crypto_provider() -> Result<()> {
    CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
        .map_err(|_| anyhow!("ERROR: failed to install default crypto provider"))?;
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
    let mut cert_reader: BufReader<File> = BufReader::new(File::open(cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(File::open(key_path)?);

    let key = rustls_pemfile::private_key(&mut key_reader)?.ok_or(anyhow!("No Private Key"))?;

    let mut tls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    tls.alpn_protocols = alpn_protocols
        .iter()
        .map(|prot| prot.alpn().into())
        .collect::<Vec<Vec<u8>>>();

    let quic_crypto = QuicServerConfig::try_from(tls)?;
    let server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_crypto));

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

    // Optional: tune transport if you want
    client.transport_config(Arc::new(TransportConfig::default()));

    Ok(client)
}
