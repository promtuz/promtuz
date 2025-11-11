use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use quinn::ServerConfig as QuinnServerConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::PrivateKeyDer;

pub fn setup_crypto_provider() -> Result<()> {
    CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
        .map_err(|_| anyhow!("ERROR: failed to install default crypto provider"))?;
    Ok(())
}

/// Builds a QUIC server configuration using a TLS certificate, private key,
/// and a list of ALPN protocols the server is willing to accept.
///
/// This function loads the TLS material from disk, constructs a
/// `rustls::ServerConfig`, attaches the provided ALPN protocol list,
/// and converts it into a `quinn::ServerConfig` suitable for creating
/// a QUIC endpoint.
///
/// # Parameters
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
/// # Returns
///
/// Returns a fully initialized [`quinn::ServerConfig`] wrapped in an
/// application-specific `QuinnServerConfig` type (or as defined in your
/// codebase).  
/// This configuration can be passed to `Endpoint::server` to create a
/// listening QUIC endpoint.
///
/// # Errors
///
/// Returns an error if:
/// - certificate or key files cannot be read or parsed
/// - TLS configuration cannot be constructed (e.g., invalid key format)
/// - ALPN configuration is invalid for the TLS backend
///
/// # Example
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
/// # Notes
///
/// * ALPN determines *what roles* this server is willing to accept, but
///   the **dialer** decides the actual role of a connection by choosing
///   the ALPN it offers during the handshake.
/// * Only inbound connections use this configuration. Outbound connections
///   must use a separate client configuration with a single ALPN.
pub fn build_server_cfg(
    cert_path: &Path,
    key_path: &Path,
    alpn_protocols: &'static [&'static str],
) -> Result<QuinnServerConfig> {
    let mut cert_reader: BufReader<File> = BufReader::new(File::open(cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(File::open(key_path)?);
    let mut keys = rustls_pemfile::ec_private_keys(&mut key_reader)
        .flatten()
        .collect::<Vec<_>>();
    let key = PrivateKeyDer::from(keys.remove(0));

    let mut tls = RustlsServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    tls.alpn_protocols = alpn_protocols
        .iter()
        .map(|prot| prot.as_bytes().to_vec())
        .collect::<Vec<Vec<u8>>>();

    let quic_crypto = QuicServerConfig::try_from(tls)?;
    let server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_crypto));

    Ok(server_cfg)
}

/// Builds a TLS client configuration for an outbound QUIC connection,
/// attaching exactly one ALPN protocol.
///
/// This configuration represents a *single connection role*.  
/// Whenever a node, resolver, or client dials another peer, it must
/// specify exactly one ALPN value that identifies the purpose of that
/// connection (e.g., `"node/1"`, `"resolver/1"`, `"peer/1"`, etc).
///
/// The server will only accept the connection if the ALPN offered here
/// matches one of the protocols in its own server-side ALPN list.
///
/// # Parameters
///
/// * `alpn_protocol`  
///   A single ALPN string representing the intended role for this
///   outbound connection.  
///   For example:
///   - `"node/1"`     → node → resolver
///   - `"resolver/1"` → resolver → resolver
///   - `"peer/1"`     → node → node
///   - `"client/1"`   → client → resolver
///
/// # Returns
///
/// A fully initialized [`rustls::ClientConfig`] that Quinn can use when
/// dialing outbound connections.  
/// This config is typically wrapped in a `quinn::ClientConfig` and
/// injected into an `Endpoint` via `set_default_client_config()`, or
/// passed explicitly when calling `connect()`.
///
/// # Errors
///
/// Returns an error if TLS configuration fails (e.g. no trusted roots,
/// unsupported ALPN, or invalid crypto provider state).
///
/// # Example
///
/// ```no_run
/// let client_cfg = build_client_cfg("node/1")?;
/// endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(client_cfg)));
/// let conn = endpoint.connect("1.2.3.4:4433".parse()?, "resolver")?.await?;
/// ```
///
/// # Notes
///
/// * Outbound connections **must** specify exactly one ALPN.
/// * ALPN chosen here defines the *role* of the connection.
/// * This is separate from the server-side ALPN accept list.
pub fn build_client_cfg(alpn_protocol: &'static str) -> Result<rustls::ClientConfig> {
    todo!()
}
