use anyhow::Result;
use anyhow::bail;
use common::PROTOCOL_VERSION;
use common::crypto::PublicKey;
use common::crypto::get_nonce;
use common::proto::Sender;
use common::proto::client_rel::HandshakeP;
use common::proto::client_rel::RelayPacket;
use common::proto::pack::Unpacker;
use ed25519_dalek::Signature;
use quinn::Connection;

use crate::relay::RelayRef;
use crate::util::systime;

/// Handles handshake linearly
pub async fn handle_handshake(
    relay: RelayRef, conn: &Connection,
) -> Result<PublicKey, anyhow::Error> {
    use HandshakeP::*;
    use RelayPacket::*;

    let order_mismatch = Handshake(ServerReject { reason: "Packet Order Mismatch".into() });

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 0. Open first bi-stream just for handshake

    let (mut tx, mut rx) = conn.accept_bi().await?;

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 1. Client must send `ClientHello`

    let Handshake(ClientHello { ipk }) = RelayPacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };
    let ipk = PublicKey::from_bytes(&ipk)?;

    let nonce = get_nonce::<32>();

    Handshake(ServerChallenge { nonce }).send(&mut tx).await?;

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 2. Client must respond with proof of his identity

    let Handshake(ClientProof { sig }) = RelayPacket::unpack(&mut rx).await? else {
        order_mismatch.send(&mut tx).await.err();
        bail!("Packet Mismatch");
    };

    let ipk_bytes = ipk.to_bytes();
    let msg = [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &nonce].concat();
    let packet = match Signature::from_slice(&sig) {
        Ok(sig) if ipk.verify_strict(&msg, &sig).is_ok() => {
            ServerAccept { timestamp: systime().as_secs() }
        },
        _ => ServerReject { reason: "Invalid Signature".into() },
    };
    Handshake(packet).send(&mut tx).await?;
    _ = tx.finish();

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 3. Register this client as connected

    let relay = relay.clone();
    let conn = conn.clone();
    relay.clients.write().insert(ipk_bytes, conn);

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    Ok(ipk)
}
