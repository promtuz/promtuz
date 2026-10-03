//! Attachment protocol negotiation by ALPN, independent of relay and MLS versions. A failure
//! never retries the handshake with a weaker offer.

use anyhow::{Result, anyhow, bail};
use quinn::{Connection, crypto::rustls::HandshakeData};

// Frozen: deriving it from PROTOCOL_VERSION would break attachment compatibility on an
// unrelated version bump.
pub(crate) const LEGACY_ALPN: &[u8] = b"peer/10";
pub(crate) const V2_ALPN: &[u8] = b"promtuz-attachment/2";

pub(crate) fn offered_alpns() -> Vec<Vec<u8>> {
    // rustls picks by server preference, so the newer grammar goes first on both endpoints.
    vec![V2_ALPN.to_vec(), LEGACY_ALPN.to_vec()]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttachmentProtocol {
    Legacy,
    V2,
}

impl AttachmentProtocol {
    pub(crate) fn from_conn(conn: &Connection) -> Result<Self> {
        // Needs an established connection. ProtoRole::from_conn would drop the version suffix.
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
    use super::*;
    use crate::test_support::transfer::handshake;

    fn side(conn: &Result<Connection, quinn::ConnectionError>) -> String {
        match conn {
            Err(_) => "refused".into(),
            Ok(conn) => AttachmentProtocol::from_conn(conn)
                .map_or("unsupported".into(), |p| format!("{p:?}")),
        }
    }

    #[tokio::test]
    async fn mixed_versions_agree_on_one_protocol_or_fail_without_a_downgrade() {
        let only = |alpn: &[u8]| vec![alpn.to_vec()];
        let unknown = b"promtuz-attachment/999".as_slice();
        let cases = [
            (only(LEGACY_ALPN), offered_alpns(), "Legacy Legacy"),
            (offered_alpns(), only(LEGACY_ALPN), "Legacy Legacy"),
            (offered_alpns(), vec![LEGACY_ALPN.to_vec(), V2_ALPN.to_vec()], "V2 V2"),
            (only(LEGACY_ALPN), only(V2_ALPN), "refused refused"),
            (only(unknown), only(unknown), "unsupported unsupported"),
        ];
        for (server, client, expected) in cases {
            let h = handshake(server.clone(), client.clone()).await;
            let got = format!("{} {}", side(&h.accepted), side(&h.dialed));
            assert_eq!(got, expected, "server {server:?}, client {client:?}");
        }
        for alpn in [None, Some(&b"peer/11"[..]), Some(&b"peer/10/extra"[..]), Some(&b"peer"[..])] {
            assert!(AttachmentProtocol::from_alpn(alpn).is_err(), "{alpn:?}");
        }
    }
}
