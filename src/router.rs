use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use quinn::Endpoint;
use quinn::ServerConfig;
use quinn::TransportConfig;
use quinn::crypto::rustls::HandshakeData;
use quinn::crypto::rustls::QuicServerConfig;
use rustls::ServerConfig as RustlsServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::PrivateKeyDer;

use crate::util::config::AppConfig;

pub fn load_tls(cert_path: &Path, key_path: &Path) -> Result<RustlsServerConfig, Box<dyn Error>> {
    let mut cert_reader: BufReader<File> = BufReader::new(File::open(cert_path)?);
    let certs = rustls_pemfile::certs(&mut cert_reader).flatten().collect();

    let mut key_reader = BufReader::new(File::open(key_path)?);
    let mut keys = rustls_pemfile::ec_private_keys(&mut key_reader).flatten().collect::<Vec<_>>();
    let key = PrivateKeyDer::from(keys.remove(0));

    let mut tls =
        RustlsServerConfig::builder().with_no_client_auth().with_single_cert(certs, key)?;

    tls.alpn_protocols = vec![b"resolver/1".to_vec(), b"node/1".to_vec(), b"client/1".to_vec()];

    Ok(tls)
}

pub async fn serve_quic(config: &AppConfig) -> Result<(), Box<dyn Error>> {
    CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
        .map_err(|_| "ERROR: failed to install default crypto provider")?;

    let tls_config = load_tls(&config.network.cert_path, &config.network.key_path)?;

    let quic_crypto = QuicServerConfig::try_from(tls_config)?;
    let mut server_cfg = ServerConfig::with_crypto(Arc::new(quic_crypto));
    server_cfg.transport = Arc::new(TransportConfig::default());

    let endpoint = Endpoint::server(server_cfg, config.network.address)?;

    println!("[!] Listening to QUIC on {:?}", endpoint.local_addr());

    while let Some(conn) = endpoint.accept().await {
        tokio::spawn(async move {
            if let Ok(connection) = conn.await {
                handle_incoming(connection).await;
            }
        });
    }

    Ok(())
}

fn get_alpn(conn: &quinn::Connection) -> Option<Vec<u8>> {
    let any = conn.handshake_data()?; // Box<dyn Any>
    let hs = any.downcast_ref::<HandshakeData>()?; // &HandshakeData
    hs.protocol.clone() // Vec<u8>
}

async fn handle_incoming(conn: quinn::Connection) {
    let alpn = get_alpn(&conn);

    match alpn.as_deref() {
        Some(b"client/1") => {
            println!("ALPN: {:?}", String::from_utf8_lossy(&alpn.unwrap()))
        },
        Some(b"node/1") => {
            println!("ALPN: {:?}", String::from_utf8_lossy(&alpn.unwrap()))
        },
        Some(b"resolver/1") => {
            println!("ALPN: {:?}", String::from_utf8_lossy(&alpn.unwrap()))
        },
        _ => conn.close(0u32.into(), b"unsupported-alpn"),
    }
}
