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
#[cfg(feature = "crypto")]
use rustls::DigitallySignedStruct;
#[cfg(feature = "crypto")]
use rustls::DistinguishedName;
use rustls::RootCertStore;
use rustls::ServerConfig as RustlsServerConfig;
#[cfg(feature = "crypto")]
use rustls::SignatureScheme;
#[cfg(feature = "crypto")]
use rustls::client::danger::HandshakeSignatureValid;
#[cfg(feature = "crypto")]
use rustls::client::danger::ServerCertVerified;
#[cfg(feature = "crypto")]
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
#[cfg(feature = "crypto")]
use rustls::pki_types::CertificateDer;
#[cfg(feature = "crypto")]
use rustls::pki_types::ServerName;
#[cfg(feature = "crypto")]
use rustls::pki_types::UnixTime;
#[cfg(feature = "crypto")]
use rustls::server::ClientHello;
#[cfg(feature = "crypto")]
use rustls::server::ResolvesServerCert;
#[cfg(feature = "crypto")]
use rustls::server::danger::ClientCertVerified;
#[cfg(feature = "crypto")]
use rustls::server::danger::ClientCertVerifier;
#[cfg(feature = "crypto")]
use rustls::sign::CertifiedKey;

/// Applied on both ends. A backgrounded phone freezes and stops its keepalives, so this bounds
/// zombie presence and moves delivery to the offline queue quickly.
pub const IDLE_TIMEOUT_SECS: u64 = 45;

/// No server keepalive: pinging every idle phone costs battery, and the idle timeout already
/// evicts dead peers.
fn default_server_transport() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(IDLE_TIMEOUT_SECS)).expect("valid IdleTimeout"),
    ));
    tc.max_concurrent_bidi_streams(VarInt::from_u32(64));
    tc.max_concurrent_uni_streams(VarInt::from_u32(64));
    tc
}

/// The 10 s keepalive refreshes the peer's idle timer and the NAT binding of a quiet client.
pub fn default_client_transport() -> TransportConfig {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(
        IdleTimeout::try_from(Duration::from_secs(IDLE_TIMEOUT_SECS)).expect("valid IdleTimeout"),
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

    // TODO(node-mtls): no client auth, so a server cannot read a connecting node's capability
    // cert. Require client certs on the node ALPNs; phones stay pseudonymous.
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

pub fn build_client_cfg(role: ProtoRole, roots: &RootCertStore) -> Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots.clone())
        .with_no_client_auth();

    tls.alpn_protocols = vec![role.alpn().into()];

    let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;

    let mut client = quinn::ClientConfig::new(Arc::new(quic_config));

    client.transport_config(Arc::new(default_client_transport()));

    Ok(client)
}

#[cfg(feature = "crypto")]
const ED25519_AID_DER: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];

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

    let empty_name: &[u8] = &[0x30, 0x00];
    let version: &[u8] = &[0xa0, 0x03, 0x02, 0x01, 0x02];
    let serial_der = der_integer(&public_key[0..8]);
    let sig_alg: &[u8] = &[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70];

    let tbs_content = [
        version,
        &serial_der,
        sig_alg,
        empty_name,
        &validity_der(),
        empty_name,
        &spki,
    ]
    .concat();

    encode_seq(&tbs_content)
}

/// Positive, minimally encoded DER INTEGER: the RFC 5280 serialNumber form.
#[cfg(feature = "crypto")]
fn der_integer(magnitude: &[u8]) -> Vec<u8> {
    let start = magnitude.iter().position(|b| *b != 0).unwrap_or(magnitude.len());
    let significant = &magnitude[start..];
    let sign_pad: &[u8] =
        if significant.first().is_none_or(|b| b & 0x80 != 0) { &[0x00] } else { &[] };
    let body = [sign_pad, significant].concat();
    [&[0x02, body.len() as u8][..], &body].concat()
}

/// Validity for a key-as-identity cert: RFC 5280's "no well-defined expiration"
/// sentinel, which must be GeneralizedTime while notBefore stays UTCTime.
#[cfg(feature = "crypto")]
fn validity_der() -> Vec<u8> {
    const NOT_BEFORE: &[u8] = b"700101000000Z";
    const NOT_AFTER: &[u8] = b"99991231235959Z";
    encode_seq(
        &[
            &[0x17, NOT_BEFORE.len() as u8][..],
            NOT_BEFORE,
            &[0x18, NOT_AFTER.len() as u8][..],
            NOT_AFTER,
        ]
        .concat(),
    )
}

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

    let certs = vec![CertificateDer::from(cert_der)];
    let signing_key: Arc<dyn rustls::sign::SigningKey> = Arc::new(Ed25519SigningKey {
        public_key,
        signing: signing_arc,
    });

    CertifiedKey::new(certs, signing_key)
}

/// Accepts any cert with an Ed25519 key and checks the TLS 1.3 handshake signature under it. No CA
/// chain, validity or name check: the caller pins the key after the handshake.
#[cfg(feature = "crypto")]
#[derive(Debug)]
pub struct Ed25519CertVerifier;

#[cfg(feature = "crypto")]
fn cert_ed25519_key(cert: &CertificateDer<'_>) -> Result<[u8; 32], rustls::Error> {
    crate::node::enroll::spki_ed25519(cert.as_ref())
        .ok_or_else(|| rustls::Error::General("peer cert is not an Ed25519 X.509".into()))
}

#[cfg(feature = "crypto")]
fn verify_tls13_ed25519(
    message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    if dss.scheme != SignatureScheme::ED25519 {
        return Err(rustls::Error::General(format!(
            "unsupported handshake signature scheme: {:?}",
            dss.scheme
        )));
    }
    let sig: [u8; 64] = dss.signature().try_into().map_err(|_| {
        rustls::Error::General("Ed25519 handshake signature must be 64 bytes".into())
    })?;
    crate::crypto::verify_ed25519(&cert_ed25519_key(cert)?, message, &sig)
        .map_err(|e| rustls::Error::General(format!("Ed25519 handshake signature failed: {e}")))?;
    Ok(HandshakeSignatureValid::assertion())
}

#[cfg(feature = "crypto")]
impl ServerCertVerifier for Ed25519CertVerifier {
    fn verify_server_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>, _ocsp_response: &[u8], _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        cert_ed25519_key(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_ed25519(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

#[cfg(feature = "crypto")]
impl ClientCertVerifier for Ed25519CertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self, end_entity: &CertificateDer<'_>, _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        cert_ed25519_key(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self, _message: &[u8], _cert: &CertificateDer<'_>, _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 not supported".into()))
    }

    fn verify_tls13_signature(
        &self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_ed25519(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
}

/// Serves the peer ALPN a self-signed, non-expiring cert over the NodeKey, since a peer dialer
/// pins `BLAKE3(SPKI) == NodeId` and nothing else; every other ALPN gets the CA-issued cert.
#[cfg(feature = "crypto")]
#[derive(Debug)]
pub struct AlpnAwareCertResolver {
    pub peer_cert: Arc<CertifiedKey>,
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

/// `node_signing` must be the key at `key_path`, so both certs carry one SPKI.
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
