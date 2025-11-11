use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;
use quinn::ServerConfig as QuinnServerConfig;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::PrivateKeyDer;

use crate::util::config::AppConfig;

pub fn setup_crypto_provider() -> Result<()> {
    CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
        .map_err(|_| anyhow!("ERROR: failed to install default crypto provider"))?;
    Ok(())
}

/// Creates Server Config for acceptor
pub fn build_server_cfg(app_cfg: &AppConfig) -> Result<QuinnServerConfig> {
    let mut cert_reader: BufReader<File> = BufReader::new(File::open(&app_cfg.network.cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(File::open(&app_cfg.network.key_path)?);
    let mut keys = rustls_pemfile::ec_private_keys(&mut key_reader).flatten().collect::<Vec<_>>();
    let key = PrivateKeyDer::from(keys.remove(0));

    let mut tls =
        RustlsServerConfig::builder().with_no_client_auth().with_single_cert(certs, key)?;

    // TODO: ALPNs should be somewhere else and systematically that so
    tls.alpn_protocols = vec![b"resolver/1".to_vec(), b"node/1".to_vec(), b"client/1".to_vec()];

    let quic_crypto = QuicServerConfig::try_from(tls)?;
    let server_cfg = QuinnServerConfig::with_crypto(Arc::new(quic_crypto));

    Ok(server_cfg)
}

/// Creates Client Config for dialer
pub fn build_client_cfg(alpn_protocol: &'static str) -> Result<rustls::ClientConfig> {
    todo!()
}
