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
    let msg = [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &*nonce].concat();
    let packet = match Signature::from_slice(&*sig) {
        Ok(sig) if ipk.verify_strict(&msg, &sig).is_ok() => {
            // Advertise our DHT NodeId so the phone can sign
            // welcome fetch/ack wrappers bound to this home. `None`
            // when DHT is disabled (those RPCs reply DhtUnavailable).
            let relay_node_id =
                relay.dht.as_ref().map(|d| common::types::bytes::Bytes(*d.node_id.as_bytes()));
            ServerHandshakeResultP::Accept { timestamp: systime().as_secs(), relay_node_id }
        },
        _ => ServerHandshakeResultP::Reject { reason: "Invalid Signature".into() },
    };
    HandshakeResult(packet).send(&mut tx).await?;
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
