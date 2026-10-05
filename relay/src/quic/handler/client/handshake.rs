use anyhow::Result;
use anyhow::bail;
use common::crypto::PublicKey;
use common::crypto::get_nonce;
use common::crypto::verify_ed25519;
use common::proto::Sender;
use common::proto::client_rel::CHandshakePacket;
use common::proto::client_rel::SHandshakePacket;
use common::proto::client_rel::client_auth_message;
use common::quic::client_auth_binding;
use common::proto::client_rel::ServerHandshakeResultP;
use common::proto::pack::Unpacker;
use common::quic::CloseReason;
use quinn::Connection;

use crate::relay::RelayRef;

pub(super) async fn handle_handshake(
    relay: RelayRef, conn: &Connection,
) -> Result<PublicKey, anyhow::Error> {
    use CHandshakePacket::*;
    use SHandshakePacket::*;

    let order_mismatch =
        HandshakeResult(ServerHandshakeResultP::Reject { reason: "Packet Order Mismatch".into() });

    let (mut tx, mut rx) = conn.accept_bi().await?;

    let Hello { ipk } = CHandshakePacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };
    let ipk = PublicKey::from_bytes(&ipk)?;

    let nonce = get_nonce::<32>().into();

    SHandshakePacket::Challenge { nonce }.send(&mut tx).await?;

    let Proof { sig } = CHandshakePacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };

    let ipk_bytes = ipk.to_bytes();

    let proof = client_auth_message(&nonce, &client_auth_binding(conn)?);
    if verify_ed25519(&ipk_bytes, &proof, &sig).is_err() {
        HandshakeResult(ServerHandshakeResultP::Reject { reason: "Invalid Signature".into() })
            .send(&mut tx)
            .await
            .err();
        bail!(
            "client({}) failed auth for ipk({})",
            conn.remote_address(),
            hex::encode(&ipk_bytes[..4])
        );
    }

    // The client binds its welcome fetch/ack signatures to this NodeId.
    let relay_node_id =
        relay.dht.as_ref().map(|d| common::types::bytes::Bytes(*d.node_id.as_bytes()));
    HandshakeResult(ServerHandshakeResultP::Accept {
        timestamp: common::utils::now_secs(),
        relay_node_id,
        assist: relay.assist_enabled,
        turn_port: relay.turn.as_ref().map(|t| t.port),
    })
    .send(&mut tx)
    .await?;
    _ = tx.finish();

    // Last connection wins: a dead app sends no FIN, so rejecting would lock the user out until
    // the stale entry idles out. The loser's `stable_id`-guarded cleanup spares the new entry.
    {
        let new_conn = conn.clone();
        let mut clients = relay.clients.write();
        if let Some(existing) = clients.get(&ipk_bytes)
            && existing.close_reason().is_none()
        {
            CloseReason::Reconnecting.close(existing);
        }
        clients.insert(ipk_bytes, new_conn);
        // A replacement connection starts idle, including a background push wake.
        relay.active_clients.write().remove(&ipk_bytes);
    }

    Ok(ipk)
}
