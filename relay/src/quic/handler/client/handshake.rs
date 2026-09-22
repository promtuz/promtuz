use anyhow::Result;
use anyhow::bail;
use common::crypto::PublicKey;
use common::crypto::get_nonce;
use common::proto::Sender;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::SHandshakePacket;
use common::proto::client_rel::client_auth_message;
use common::quic::client_auth_binding;
use common::proto::client_rel::ServerHandshakeResultP;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use ed25519_dalek::Signature;
use quinn::Connection;

use crate::relay::RelayRef;
use crate::util::systime;

fn verify_client_proof(ipk: &PublicKey, nonce: &[u8; 32], binding: &[u8; 32], sig: &[u8]) -> bool {
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    ipk.verify_strict(&client_auth_message(nonce, binding), &sig).is_ok()
}

/// Handles handshake linearly
pub(super) async fn handle_handshake(
    relay: RelayRef, conn: &Connection,
) -> Result<PublicKey, anyhow::Error> {
    use CHandshakePacket::*;
    use SHandshakePacket::*;

    let order_mismatch =
        HandshakeResult(ServerHandshakeResultP::Reject { reason: "Packet Order Mismatch".into() });

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 0. Open first bi-stream just for handshake

    let (mut tx, mut rx) = conn.accept_bi().await?;

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 1. Client must send `ClientHello`

    let Hello { ipk } = CHandshakePacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };
    let ipk = PublicKey::from_bytes(&ipk)?;

    let nonce = get_nonce::<32>().into();

    SHandshakePacket::Challenge { nonce }.send(&mut tx).await?;

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 2. Client must respond with proof of his identity

    let Proof { sig } = CHandshakePacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };

    let ipk_bytes = ipk.to_bytes();

    if !verify_client_proof(&ipk, &nonce, &client_auth_binding(conn)?, &*sig) {
        HandshakeResult(ServerHandshakeResultP::Reject { reason: "Invalid Signature".into() })
            .send(&mut tx)
            .await
            .err();
        bail!("client({}) failed auth for ipk({ipk:?})", conn.remote_address());
    }

    // Advertise our DHT NodeId so the phone can sign welcome fetch/ack wrappers
    // bound to this home. `None` when DHT is disabled (those RPCs reply
    // DhtUnavailable).
    let relay_node_id =
        relay.dht.as_ref().map(|d| common::types::bytes::Bytes(*d.node_id.as_bytes()));
    HandshakeResult(ServerHandshakeResultP::Accept {
        timestamp: systime().as_secs(),
        relay_node_id,
        assist: relay.assist_enabled,
        turn_port: relay.turn.as_ref().map(|t| t.port),
    })
    .send(&mut tx)
    .await?;
    _ = tx.finish();

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 3. Register this client as connected — last-connection-wins.
    //
    // QUIC gets no FIN when an app dies, so a stale entry's `close_reason()`
    // stays `None` until its idle timeout: rejecting on a live-looking entry
    // would lock the user out for that window. Displacing is safe because
    // `remove_client_if_same` is stable_id-guarded, so the loser's cleanup
    // no-ops rather than evicting the new connection.
    {
        let new_conn = conn.clone();
        let mut clients = relay.clients.write();
        if let Some(existing) = clients.get(&ipk_bytes)
            && existing.close_reason().is_none()
        {
            CloseReason::Reconnecting.close(existing);
        }
        clients.insert(ipk_bytes, new_conn);
    }

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    Ok(ipk)
}

#[cfg(test)]
mod tests {
    use common::crypto::get_signing_key;
    use ed25519_dalek::Signer;

    use super::*;

    const BINDING: [u8; 32] = [9u8; 32];

    fn sign_nonce(key: &ed25519_dalek::SigningKey, nonce: &[u8; 32]) -> [u8; 64] {
        key.sign(&client_auth_message(nonce, &BINDING)).to_bytes()
    }

    #[test]
    fn accepts_a_genuine_proof() {
        let key = get_signing_key();
        let nonce = [7u8; 32];
        let sig = sign_nonce(&key, &nonce);
        assert!(verify_client_proof(&key.verifying_key(), &nonce, &BINDING, &sig));
    }

    #[test]
    fn rejects_garbage_signature() {
        let victim = get_signing_key().verifying_key();
        let nonce = [7u8; 32];
        assert!(!verify_client_proof(&victim, &nonce, &BINDING, &[0u8; 64]));
        assert!(!verify_client_proof(&victim, &nonce, &BINDING, &[0xffu8; 64]));
    }

    #[test]
    fn rejects_proof_from_a_different_key() {
        let attacker = get_signing_key();
        let victim = get_signing_key().verifying_key();
        let nonce = [7u8; 32];
        let sig = sign_nonce(&attacker, &nonce);
        assert!(!verify_client_proof(&victim, &nonce, &BINDING, &sig));
    }

    #[test]
    fn rejects_proof_for_a_different_nonce() {
        let key = get_signing_key();
        let sig = sign_nonce(&key, &[1u8; 32]);
        assert!(!verify_client_proof(&key.verifying_key(), &[2u8; 32], &BINDING, &sig));
    }

    /// The proof is for one TLS session: the same nonce signed under another
    /// connection's keying material is a replay, and fails.
    #[test]
    fn rejects_proof_made_on_another_connection() {
        let key = get_signing_key();
        let nonce = [7u8; 32];
        let sig = sign_nonce(&key, &nonce);
        assert!(!verify_client_proof(&key.verifying_key(), &nonce, &[8u8; 32], &sig));
    }

    #[test]
    fn rejects_malformed_signature_length() {
        let key = get_signing_key();
        let nonce = [7u8; 32];
        assert!(!verify_client_proof(&key.verifying_key(), &nonce, &BINDING, &[]));
        assert!(!verify_client_proof(&key.verifying_key(), &nonce, &BINDING, &[0u8; 63]));
    }
}
