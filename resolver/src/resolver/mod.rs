use std::fmt;
use std::path::Path;
use std::sync::Arc;

use common::crypto::verify_ed25519;
use common::node::capability::NodeCapabilities;
use common::node::enroll::cert_capabilities;
use common::node::enroll::verify_leaf;
use common::proto::RelayId;
use common::proto::relay_res::LifetimeP;
use common::proto::relay_res::NODE_HELLO_EXPORTER_LABEL;
use common::proto::relay_res::gateway_hello_signing_input;
use common::proto::relay_res::relay_heartbeat_signing_input;
use common::proto::relay_res::relay_hello_signing_input;
use common::quic::CloseReason;
use common::quic::id::NodeId;
use common::quic::session_binding;
use common::utils::now_ms;
use common::warn;
use parking_lot::RwLock;
use quinn::Connection;
use quinn::Endpoint;

use crate::resolver::relays::Directory;
use crate::util::config::AppConfig;

pub mod relays;
pub mod rpc;

pub type ResolverRef = Arc<Resolver>;

const MAX_RELAYS: usize = 1024;

const MAX_GATEWAYS: usize = 64;

const HELLO_MAX_SKEW_MS: u128 = 60_000;

pub struct Resolver {
    pub cfg: AppConfig,
    pub endpoint: Endpoint,
    pub relays: Arc<Directory>,
    pub gateways: Arc<Directory>,
    relays_response: RwLock<Option<(u64, Arc<Vec<u8>>)>>,
}

impl Resolver {
    pub fn new(cfg: AppConfig, endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            relays: Directory::new("relay", MAX_RELAYS, true),
            gateways: Directory::new("gateway", MAX_GATEWAYS, false),
            relays_response: RwLock::new(None),
            cfg,
        }
    }

    /// Verifies the id-key binding, the session-bound signature, freshness and a gateway's cert,
    /// then admits the sender to the directory its hello names.
    pub fn register(
        &self, conn: &Arc<Connection>, hello: &LifetimeP,
    ) -> Result<&Arc<Directory>, CloseReason> {
        let addr = conn.remote_address();
        let binding = session_binding(conn, NODE_HELLO_EXPORTER_LABEL).map_err(|e| {
            warn!("hello from {addr} rejected: no session binding: {e}");
            CloseReason::BadSignature
        })?;
        let (dir, id, pubkey, timestamp, sig, msg) = match hello {
            LifetimeP::RelayHello { relay_id, pubkey, timestamp, sig } => {
                let msg = relay_hello_signing_input(relay_id, &pubkey.0, *timestamp, &binding);
                (&self.relays, relay_id, pubkey, *timestamp, sig, msg)
            },
            LifetimeP::GatewayHello { gateway_id, pubkey, timestamp, sig, .. } => {
                let msg = gateway_hello_signing_input(gateway_id, &pubkey.0, *timestamp, &binding);
                (&self.gateways, gateway_id, pubkey, *timestamp, sig, msg)
            },
            _ => return Err(CloseReason::PacketMismatch),
        };

        let label = format_args!("{} hello from {addr}", dir.kind);
        verify_signed_packet(label, id, &pubkey.0, &sig.0, &msg, timestamp, now_ms().into())?;
        if let LifetimeP::GatewayHello { cert, .. } = hello {
            verify_gateway_cert(label, &self.cfg.network.root_ca_path, cert, id, &pubkey.0)?;
        }
        dir.admit(*id, *pubkey, conn.clone())?;
        Ok(dir)
    }

    pub fn verify_heartbeat(
        &self, conn: &Arc<Connection>, packet: &LifetimeP,
    ) -> Result<(), CloseReason> {
        let LifetimeP::RelayHeartbeat { relay_id, pubkey, timestamp, sig, .. } = packet else {
            return Err(CloseReason::PacketMismatch);
        };

        let addr = conn.remote_address();
        let msg = relay_heartbeat_signing_input(relay_id, &pubkey.0, *timestamp);
        let label = format_args!("relay heartbeat from {addr}");
        let now = now_ms().into();
        verify_signed_packet(label, relay_id, &pubkey.0, &sig.0, &msg, *timestamp, now)?;

        if !self.relays.touch(relay_id, conn) {
            warn!(
                "relay heartbeat from {addr} rejected: {relay_id} not registered on this connection"
            );
            return Err(CloseReason::PacketMismatch);
        }
        Ok(())
    }

    pub fn close(&self) {
        self.relays.close_all();
        self.gateways.close_all();
    }
}

/// What a relay checks when it dials a gateway: a CA-issued cert for this id and key that carries
/// `PUSH_GATEWAY`.
fn verify_gateway_cert(
    label: fmt::Arguments, ca_path: &Path, cert: &[u8], id: &RelayId, pubkey: &[u8; 32],
) -> Result<(), CloseReason> {
    if let Err(e) = verify_leaf(cert, ca_path, id, pubkey) {
        warn!("{label} rejected: {e:#}");
        return Err(CloseReason::BadSignature);
    }
    if !cert_capabilities(cert).is_some_and(|c| c.contains(NodeCapabilities::PUSH_GATEWAY)) {
        warn!("{label} rejected: the cert lacks PUSH_GATEWAY");
        return Err(CloseReason::UnsupportedRole);
    }
    Ok(())
}

fn verify_signed_packet(
    label: fmt::Arguments, id: &RelayId, pubkey: &[u8; 32], sig: &[u8; 64], signing_input: &[u8],
    timestamp: u128, now: u128,
) -> Result<(), CloseReason> {
    if &NodeId::new(pubkey) != id {
        warn!("{label} rejected: id does not match pubkey");
        return Err(CloseReason::BadSignature);
    }

    if verify_ed25519(pubkey, signing_input, sig).is_err() {
        warn!("{label} rejected: invalid signature");
        return Err(CloseReason::BadSignature);
    }

    let skew = now.abs_diff(timestamp);
    if skew > HELLO_MAX_SKEW_MS {
        warn!("{label} rejected: stale timestamp ({skew}ms skew)");
        return Err(CloseReason::StaleTimestamp);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use common::crypto::SigningKey;
    use ed25519_dalek::Signer as _;

    use super::*;

    #[test]
    fn a_hello_verifies_only_from_its_key_within_the_skew_window() {
        let now = 1_800_000_000_000;
        let key = SigningKey::from_bytes(&[3; 32]);
        let pubkey = key.verifying_key().to_bytes();
        let own = NodeId::new(pubkey);
        let check = |id: RelayId, timestamp: u128, forged: bool| {
            let msg = relay_hello_signing_input(&id, &pubkey, timestamp, &[0x5B; 32]);
            let mut sig = key.sign(&msg).to_bytes();
            sig[0] ^= u8::from(forged);
            verify_signed_packet(format_args!("hello"), &id, &pubkey, &sig, &msg, timestamp, now)
                .err()
                .map(|reason| reason.code())
        };

        let skew = HELLO_MAX_SKEW_MS;
        let (bad_signature, stale) =
            (Some(CloseReason::BadSignature.code()), Some(CloseReason::StaleTimestamp.code()));
        let cases = [
            (own, now - skew, false, None),
            (own, now + skew, false, None),
            (own, now - skew - 1, false, stale),
            (own, now + skew + 1, false, stale),
            (own, now, true, bad_signature),
            (NodeId::new([9; 32]), now, false, bad_signature),
        ];
        for (id, timestamp, forged, want) in cases {
            assert_eq!(check(id, timestamp, forged), want, "{id} at {timestamp}, forged: {forged}");
        }
    }

    #[test]
    fn a_gateway_registers_only_with_a_ca_issued_push_gateway_cert() -> anyhow::Result<()> {
        use common::node::capability::CAPABILITY_OID;
        use rcgen::CertificateParams;
        use rcgen::CustomExtension;
        use rcgen::KeyPair;
        let _ = common::quic::config::setup_crypto_provider();
        let new_key = || KeyPair::generate_for(&rcgen::PKCS_ED25519);
        let (ca_key, gateway, other) = (new_key()?, new_key()?, new_key()?);
        let mut ca = CertificateParams::default();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let dir = tempfile::tempdir()?;
        let ca_path = dir.path().join("ca.pem");
        std::fs::write(&ca_path, ca.self_signed(&ca_key)?.pem())?;
        let issuer = rcgen::Issuer::new(ca, &ca_key);

        let public = |key: &KeyPair| -> anyhow::Result<[u8; 32]> { Ok(key.public_key_raw().try_into()?) };
        let id = |key: &KeyPair| NodeId::new(key.public_key_raw());
        let leaf = |key: &KeyPair, name: NodeId, caps: Option<NodeCapabilities>, by_ca: bool| -> anyhow::Result<Vec<u8>> {
            let mut params = CertificateParams::new(vec![name.to_string()])?;
            let stamp =
                caps.map(|caps| CustomExtension::from_oid_content(CAPABILITY_OID, caps.encode()));
            params.custom_extensions.extend(stamp);
            let cert = if by_ca { params.signed_by(key, &issuer) } else { params.self_signed(key) };
            Ok(cert?.der().to_vec())
        };
        let (push, relay) = (Some(NodeCapabilities::PUSH_GATEWAY), Some(NodeCapabilities::RELAY));
        let bad = Some(CloseReason::BadSignature.code());
        let not_a_gateway = Some(CloseReason::UnsupportedRole.code());
        let cases = [
            ("a CA-issued gateway cert", leaf(&gateway, id(&gateway), push, true)?, None),
            ("a relay cert", leaf(&gateway, id(&gateway), relay, true)?, not_a_gateway),
            ("no capabilities", leaf(&gateway, id(&gateway), None, true)?, not_a_gateway),
            ("another key's cert", leaf(&other, id(&other), push, true)?, bad),
            ("another id's cert", leaf(&gateway, id(&other), push, true)?, bad),
            ("a self-signed cert", leaf(&gateway, id(&gateway), push, false)?, bad),
            ("not a cert", vec![0x30, 0x03, 0x02, 0x01, 0x01], bad),
        ];
        let (gateway_id, gateway_key) = (id(&gateway), public(&gateway)?);
        for (why, cert, want) in cases {
            let label = format_args!("{why}");
            let got = verify_gateway_cert(label, &ca_path, &cert, &gateway_id, &gateway_key);
            assert_eq!(got.err().map(|reason| reason.code()), want, "{why}");
        }
        Ok(())
    }
}
