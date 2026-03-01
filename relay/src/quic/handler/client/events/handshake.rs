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
use ed25519_dalek::Signature;
use quinn::Connection;

use crate::relay::RelayRef;
use crate::util::systime;

/// Handles handshake linearly
pub async fn handle_handshake(
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
            ServerHandshakeResultP::Accept { timestamp: systime().as_secs() }
        },
        _ => ServerHandshakeResultP::Reject { reason: "Invalid Signature".into() },
    };
    HandshakeResult(packet).send(&mut tx).await?;
    _ = tx.finish();

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 3. Register this client as connected

    let relay = relay.clone();
    let conn = conn.clone();
    relay.clients.write().insert(ipk_bytes, conn);

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    Ok(ipk)
}
