//! First-frame mutual IPK pin on every transfer stream: the cert's SPKI is a TLS sub-key, so each
//! side proves its IPK vouches for the key this connection presented. Serving authorizes per file.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use common::crypto::verify_ed25519;

use crate::quic::peer_config::extract_peer_tls_pubkey;
use crate::quic::peer_config::ipk_binding_message;
use crate::transfer::wire;

pub fn local_auth() -> Result<wire::Auth> {
    use crate::data::identity::{Identity, IdentitySigner};
    let ipk = Identity::local_ipk().ok_or_else(|| anyhow!("no identity"))?;
    let tls_pub = IdentitySigner::tls_subkey()?.verifying_key().to_bytes();
    let sig = IdentitySigner::sign(&ipk_binding_message(&tls_pub))?.to_bytes();
    Ok(wire::Auth { ipk, tls_pub, sig })
}

/// Requiring this connection's own TLS key makes a captured Auth fail on any other connection.
/// Contact status is not identity: group members transfer without pairing.
pub fn verify_auth(a: &wire::Auth, expected: [u8; 32], conn_tls_pub: [u8; 32]) -> Result<()> {
    if a.ipk != expected {
        bail!("peer ipk mismatch");
    }
    if a.tls_pub != conn_tls_pub {
        bail!("tls_pub is not the connection's cert key");
    }
    verify_ed25519(&a.ipk, &ipk_binding_message(&a.tls_pub), &a.sig)
        .map_err(|e| anyhow!("ipk binding: {e}"))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("transfer authentication failed: {0}")]
pub(crate) struct AuthenticationFailed(String);

pub async fn exchange(
    conn: &quinn::Connection, s: &mut quinn::SendStream, r: &mut quinn::RecvStream,
    expected: [u8; 32], local: &wire::Auth,
) -> Result<()> {
    wire::write_frame(s, local).await?;
    let peer: wire::Auth = wire::read_frame_limited(r, wire::AUTH_FRAME_LIMIT).await?;
    let conn_tls_pub = extract_peer_tls_pubkey(conn)
        .ok_or_else(|| AuthenticationFailed("no verifiable peer cert".into()))?;
    verify_auth(&peer, expected, conn_tls_pub)
        .map_err(|e| AuthenticationFailed(e.to_string()).into())
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::test_support::transfer::identity;

    #[test]
    fn a_binding_authenticates_only_its_identity_on_the_connection_that_presented_its_key() {
        let auth = identity(31);
        let other_key = SigningKey::from_bytes(&[39; 32]).verifying_key().to_bytes();
        let swapped = wire::Auth { tls_pub: other_key, ..auth.clone() };
        let cases = [
            (&auth, auth.ipk, auth.tls_pub, true, "valid, and no contact row is needed"),
            (&auth, [9; 32], auth.tls_pub, false, "not the identity this link expects"),
            (&auth, auth.ipk, [0xee; 32], false, "captured, replayed on another connection"),
            (&swapped, auth.ipk, other_key, false, "signed over another key"),
        ];
        for (auth, expected, conn_key, ok, why) in cases {
            assert_eq!(verify_auth(auth, expected, conn_key).is_ok(), ok, "{why}");
        }
    }
}
