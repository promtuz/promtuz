//! Attachment grammar negotiation, independent of relay and MLS versions.
//!
//! Both choices travel in one TLS handshake. The selected ALPN is protected
//! by that handshake, then transfer Auth binds its TLS key to the expected
//! identity. An error never triggers another handshake with a weaker offer.

use anyhow::{Result, anyhow, bail};
use quinn::{Connection, crypto::rustls::HandshakeData};

// Freeze the deployed grammar's identifier. Deriving this from the relay's
// PROTOCOL_VERSION would silently retire attachment compatibility on its next
// otherwise-unrelated version bump.
pub(crate) const LEGACY_ALPN: &[u8] = b"peer/10";
pub(crate) const V2_ALPN: &[u8] = b"promtuz-attachment/2";

pub(crate) fn offered_alpns() -> Vec<Vec<u8>> {
    // rustls uses server preference, so keep the newer grammar first on both
    // endpoints. A released endpoint intersects this list at peer/10.
    vec![V2_ALPN.to_vec(), LEGACY_ALPN.to_vec()]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentProtocol {
    Legacy,
    V2,
}

impl AttachmentProtocol {
    pub(crate) fn from_conn(conn: &Connection) -> Result<Self> {
        // Call only on an awaited, fully established connection. Do not use
        // ProtoRole::from_conn: it deliberately discards the version suffix.
        let data = conn.handshake_data().ok_or_else(|| anyhow!("missing peer handshake data"))?;
        let data = data
            .downcast_ref::<HandshakeData>()
            .ok_or_else(|| anyhow!("unexpected peer handshake data"))?;
        Self::from_alpn(data.protocol.as_deref())
    }

    fn from_alpn(alpn: Option<&[u8]>) -> Result<Self> {
        match alpn {
            Some(LEGACY_ALPN) => Ok(Self::Legacy),
            Some(V2_ALPN) => Ok(Self::V2),
            _ => bail!("unsupported attachment protocol"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv6Addr, time::Duration};

    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::quic::peer_config::test_peer_configs_with_protocols;

    /// Independent TLS keys and ALPN lists model both application versions
    /// without consulting the process-global identity or database.
    async fn connect(
        server_protocols: Vec<Vec<u8>>, client_protocols: Vec<Vec<u8>>,
    ) -> (
        Result<Connection, quinn::ConnectionError>,
        Result<Connection, quinn::ConnectionError>,
        quinn::Endpoint,
        quinn::Endpoint,
    ) {
        let _ = common::quic::config::setup_crypto_provider();
        let (server, _) = test_peer_configs_with_protocols(
            &SigningKey::from_bytes(&[0x35; 32]),
            server_protocols,
        )
        .unwrap();
        let (_, client) = test_peer_configs_with_protocols(
            &SigningKey::from_bytes(&[0x36; 32]),
            client_protocols,
        )
        .unwrap();
        let server_ep = quinn::Endpoint::server(server, (Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        let mut client_ep = quinn::Endpoint::client((Ipv6Addr::LOCALHOST, 0).into()).unwrap();
        client_ep.set_default_client_config(client);
        let dial = client_ep.connect(server_ep.local_addr().unwrap(), "peer").unwrap();
        let (accepted, dialed) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(async { server_ep.accept().await.unwrap().accept().unwrap().await }, dial,)
        })
        .await
        .expect("local TLS negotiation must be bounded");
        (accepted, dialed, server_ep, client_ep)
    }

    #[tokio::test]
    async fn old_and_new_endpoints_select_legacy_in_both_tls_roles() {
        for (server, client) in [
            (vec![LEGACY_ALPN.to_vec()], offered_alpns()),
            (offered_alpns(), vec![LEGACY_ALPN.to_vec()]),
        ] {
            let (accepted, dialed, _server_ep, _client_ep) = connect(server, client).await;
            let accepted = accepted.unwrap();
            let dialed = dialed.unwrap();
            assert_eq!(
                AttachmentProtocol::from_conn(&accepted).unwrap(),
                AttachmentProtocol::Legacy
            );
            assert_eq!(AttachmentProtocol::from_conn(&dialed).unwrap(), AttachmentProtocol::Legacy);
        }
    }

    #[tokio::test]
    async fn new_endpoints_select_v2_using_server_preference() {
        // Reversing the client's preference must not change a new server's
        // choice. This also checks the exact ALPN reported at both ends.
        let (accepted, dialed, _server_ep, _client_ep) =
            connect(offered_alpns(), vec![LEGACY_ALPN.to_vec(), V2_ALPN.to_vec()]).await;
        assert_eq!(
            AttachmentProtocol::from_conn(&accepted.unwrap()).unwrap(),
            AttachmentProtocol::V2
        );
        assert_eq!(
            AttachmentProtocol::from_conn(&dialed.unwrap()).unwrap(),
            AttachmentProtocol::V2
        );
    }

    #[tokio::test]
    async fn disjoint_protocols_fail_without_a_legacy_retry() {
        let (accepted, dialed, _server_ep, _client_ep) =
            connect(vec![LEGACY_ALPN.to_vec()], vec![V2_ALPN.to_vec()]).await;
        assert!(accepted.is_err());
        assert!(dialed.is_err());
    }

    #[tokio::test]
    async fn established_unknown_alpn_is_rejected_by_attachment_dispatch() {
        let unknown = vec![b"promtuz-attachment/999".to_vec()];
        let (accepted, dialed, _server_ep, _client_ep) = connect(unknown.clone(), unknown).await;
        assert!(AttachmentProtocol::from_conn(&accepted.unwrap()).is_err());
        assert!(AttachmentProtocol::from_conn(&dialed.unwrap()).is_err());
    }

    #[test]
    fn missing_or_near_match_alpn_is_not_legacy() {
        for alpn in [None, Some(&b"peer/11"[..]), Some(&b"peer/10/extra"[..]), Some(&b"peer"[..])] {
            assert!(AttachmentProtocol::from_alpn(alpn).is_err());
        }
    }
}
