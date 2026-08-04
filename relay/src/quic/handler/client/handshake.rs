use anyhow::Result;
use anyhow::bail;
use common::PROTOCOL_VERSION;
use common::crypto::PublicKey;
use common::crypto::get_nonce;
use common::proto::Sender;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::SHandshakePacket;
use common::proto::client_rel::ServerHandshakeResultP;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use ed25519_dalek::Signature;
use quinn::Connection;

use crate::relay::RelayRef;
use crate::util::systime;

/// Canonical bytes a client signs to prove possession of its identity key.
///
/// Kept as a named function so the transcript exists in exactly one place on
/// this side of the wire. libcore builds the same bytes at
/// `libcore/src/quic/server.rs`; the two are still textually duplicated, which
/// is a known wart — the transcript also binds no responder identity and no TLS
/// channel, so a relay can forward another relay's challenge and replay the
/// answer. Fixing that means changing the transcript on both sides and bumping
/// `PROTOCOL_VERSION`; it is deliberately out of scope here.
fn client_auth_message(nonce: &[u8; 32]) -> Vec<u8> {
    [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), nonce].concat()
}

/// Verify a client's `Proof` against the challenge we issued.
///
/// Split out of [`handle_handshake`] so the accept/reject decision is unit
/// testable without standing up a QUIC connection — the absence of any test
/// over this predicate is why an unconditional-registration bug survived here.
fn verify_client_proof(ipk: &PublicKey, nonce: &[u8; 32], sig: &[u8]) -> bool {
    let Ok(sig) = Signature::from_slice(sig) else {
        return false;
    };
    ipk.verify_strict(&client_auth_message(nonce), &sig).is_ok()
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

    // The verification result MUST gate control flow, not merely select a reply
    // variant. It previously did the latter: the `match` produced an
    // `Accept`/`Reject` value, the packet was sent, and registration below ran
    // unconditionally — so a peer that sent 64 arbitrary bytes was handed a
    // `Reject` *and* an authenticated session under any IPK it named. An IPK is
    // a public address, so that was a full impersonation primitive for any host
    // that could reach the port. Every early return past this point is
    // load-bearing; do not collapse them back into an expression.
    if !verify_client_proof(&ipk, &nonce, &*sig) {
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
    })
    .send(&mut tx)
    .await?;
    _ = tx.finish();

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 3. Register this client as connected — last-connection-wins.
    //
    // The peer just proved ownership of this IPK, so a pre-existing entry is a
    // superseded session: almost always the same user reconnecting (app
    // restart, network flap) while the previous QUIC connection still lingers
    // in the map — QUIC gets no FIN when an app dies, so the old conn's
    // `close_reason()` stays `None` until its own idle timeout elapses. The old
    // "reject the new connection while an entry looks live" policy therefore
    // locked the user out of reconnecting for that whole window.
    //
    // Instead we close the stale connection and let the new one take over.
    // Safe because the disconnect cleanup (`remove_client_if_same`) is
    // stable_id-guarded: the displaced connection's cleanup finds a different
    // entry under this IPK and no-ops, so it cannot evict the freshly
    // registered connection.
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

    fn sign_nonce(key: &ed25519_dalek::SigningKey, nonce: &[u8; 32]) -> [u8; 64] {
        key.sign(&client_auth_message(nonce)).to_bytes()
    }

    #[test]
    fn accepts_a_genuine_proof() {
        let key = get_signing_key();
        let nonce = [7u8; 32];
        let sig = sign_nonce(&key, &nonce);
        assert!(verify_client_proof(&key.verifying_key(), &nonce, &sig));
    }

    /// The regression test for the auth bypass: a peer that names a victim's
    /// IPK and sends arbitrary bytes must be rejected. Before the fix the
    /// verifier's answer was discarded and the session was registered anyway.
    #[test]
    fn rejects_garbage_signature() {
        let victim = get_signing_key().verifying_key();
        let nonce = [7u8; 32];
        assert!(!verify_client_proof(&victim, &nonce, &[0u8; 64]));
        assert!(!verify_client_proof(&victim, &nonce, &[0xffu8; 64]));
    }

    /// A signature that is valid — but made by a different key — must not
    /// authenticate the claimed identity.
    #[test]
    fn rejects_proof_from_a_different_key() {
        let attacker = get_signing_key();
        let victim = get_signing_key().verifying_key();
        let nonce = [7u8; 32];
        let sig = sign_nonce(&attacker, &nonce);
        assert!(!verify_client_proof(&victim, &nonce, &sig));
    }

    /// Replay across challenges: a proof captured for one nonce must not
    /// satisfy a different one. (The transcript still binds no responder
    /// identity — see `client_auth_message` — so this covers replay to the
    /// *same* relay only.)
    #[test]
    fn rejects_proof_for_a_different_nonce() {
        let key = get_signing_key();
        let sig = sign_nonce(&key, &[1u8; 32]);
        assert!(!verify_client_proof(&key.verifying_key(), &[2u8; 32], &sig));
    }

    /// A wrong-length signature must fail closed rather than panic — the wire
    /// field is attacker-sized.
    #[test]
    fn rejects_malformed_signature_length() {
        let key = get_signing_key();
        let nonce = [7u8; 32];
        assert!(!verify_client_proof(&key.verifying_key(), &nonce, &[]));
        assert!(!verify_client_proof(&key.verifying_key(), &nonce, &[0u8; 63]));
    }
}
