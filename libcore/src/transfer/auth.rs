//! First-frame mutual IPK pin on every transfer stream. The peer cert's SPKI
//! is a derived TLS sub-key, not the long-term IPK, so each side proves its
//! IPK vouches for the TLS key this connection actually presented.

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use crate::quic::peer_config::extract_peer_tls_pubkey;
use crate::quic::peer_config::ipk_binding_message;
use crate::quic::peer_config::verify_ipk_binding;
use crate::transfer::wire;

/// Our half of the handshake. The TLS sub-key is the same derivation the
/// peer cert embeds ([`crate::data::identity::IdentitySigner::tls_subkey`]),
/// so the peer can match it against the connection's presented cert.
pub fn local_auth() -> Result<wire::Auth> {
    use crate::data::identity::{Identity, IdentitySigner};
    let ipk = Identity::get().ok_or_else(|| anyhow!("no identity"))?.ipk();
    let tls_pub = IdentitySigner::tls_subkey()?.verifying_key().to_bytes();
    let sig = IdentitySigner::sign(&ipk_binding_message(&tls_pub))?.to_bytes();
    Ok(wire::Auth { ipk, tls_pub, sig })
}

/// Accept the peer's half only if the claimed IPK is the peer we expect, its
/// vouched TLS key is the one THIS connection presented (a captured Auth
/// replayed over another connection fails here), the binding signature
/// verifies, and the IPK is a paired contact.
pub fn verify_auth(a: &wire::Auth, expected: [u8; 32], conn_tls_pub: [u8; 32]) -> Result<()> {
    if a.ipk != expected {
        bail!("peer ipk mismatch");
    }
    if a.tls_pub != conn_tls_pub {
        bail!("tls_pub is not the connection's cert key");
    }
    verify_ipk_binding(&a.ipk, &a.tls_pub, &a.sig).map_err(|e| anyhow!("ipk binding: {e}"))?;
    if !crate::data::contact::Contact::is_paired(&a.ipk) {
        bail!("peer not a paired contact");
    }
    Ok(())
}

/// Run the mutual handshake on a fresh bi-stream: write `local`, read the
/// peer's, verify it against the live connection. `local` is a parameter
/// rather than `local_auth()` inline so a test can drive two distinct
/// identities in one process.
pub async fn exchange(
    conn: &quinn::Connection, s: &mut quinn::SendStream, r: &mut quinn::RecvStream,
    expected: [u8; 32], local: &wire::Auth,
) -> Result<()> {
    wire::write_frame(s, local).await?;
    let peer: wire::Auth = wire::read_frame(r).await?;
    let conn_tls_pub =
        extract_peer_tls_pubkey(conn).ok_or_else(|| anyhow!("no verifiable peer cert"))?;
    verify_auth(&peer, expected, conn_tls_pub)
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::*;
    use crate::data::contact::Contact;

    fn test_db() {
        let dir = std::env::temp_dir().join("promtuz-transfers-test");
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("PROMTUZ_DATA_DIR", &dir) }; // set_var is unsafe in edition 2024
    }

    /// Auth whose binding sig is genuinely produced by `ipk_seed`'s key over
    /// `tls_seed`'s public key.
    fn auth_for(ipk_seed: [u8; 32], tls_seed: [u8; 32]) -> wire::Auth {
        let ipk_key = SigningKey::from_bytes(&ipk_seed);
        let tls_pub = SigningKey::from_bytes(&tls_seed).verifying_key().to_bytes();
        let sig = ipk_key.sign(&ipk_binding_message(&tls_pub)).to_bytes();
        wire::Auth { ipk: ipk_key.verifying_key().to_bytes(), tls_pub, sig }
    }

    fn pair(ipk: [u8; 32]) {
        Contact::save_pending(ipk, "peer".into()).unwrap();
        Contact::mark_paired(&ipk);
    }

    #[test]
    fn accepts_paired_peer_with_valid_binding() {
        test_db();
        let a = auth_for([31u8; 32], [32u8; 32]);
        pair(a.ipk);
        verify_auth(&a, a.ipk, a.tls_pub).unwrap();
    }

    #[test]
    fn rejects_unexpected_ipk() {
        test_db();
        let a = auth_for([33u8; 32], [34u8; 32]);
        pair(a.ipk);
        assert!(verify_auth(&a, [9u8; 32], a.tls_pub).is_err());
    }

    #[test]
    fn rejects_tls_key_the_connection_did_not_present() {
        test_db();
        // A valid captured Auth replayed over a connection whose cert key differs.
        let a = auth_for([35u8; 32], [36u8; 32]);
        pair(a.ipk);
        assert!(verify_auth(&a, a.ipk, [0xeeu8; 32]).is_err());
    }

    #[test]
    fn rejects_invalid_binding_sig() {
        test_db();
        // Sig is over a DIFFERENT tls_pub than the Auth claims.
        let mut a = auth_for([37u8; 32], [38u8; 32]);
        a.tls_pub = SigningKey::from_bytes(&[39u8; 32]).verifying_key().to_bytes();
        pair(a.ipk);
        assert!(verify_auth(&a, a.ipk, a.tls_pub).is_err());
    }

    #[test]
    fn rejects_unpaired_peer() {
        test_db();
        let a = auth_for([40u8; 32], [41u8; 32]); // never saved as a contact
        assert!(verify_auth(&a, a.ipk, a.tls_pub).is_err());
    }
}
