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
            ServerHandshakeResultP::Accept { timestamp: systime().as_secs() }
        },
        _ => ServerHandshakeResultP::Reject { reason: "Invalid Signature".into() },
    };
    HandshakeResult(packet).send(&mut tx).await?;
    _ = tx.finish();

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    // 3. Register this client as connected.
    //
    // Race-safety: a second valid handshake from the same identity must NOT
    // silently displace the live entry — that would orphan the previous
    // connection in the map and, worse, the eventual disconnect cleanup of
    // *either* connection would unconditionally `remove(ipk)` and leave the
    // user unreachable on this relay (see `handle_client`'s cleanup, which
    // is now ptr-equality-guarded).
    //
    // Policy: if a live entry already exists for this IPK, reject the new
    // connection with `AlreadyConnected`. Only sweep entries whose
    // connection has already been torn down.
    {
        let relay = relay.clone();
        let new_conn = conn.clone();
        let mut clients = relay.clients.write();
        if let Some(existing) = clients.get(&ipk_bytes) {
            if existing.close_reason().is_none() {
                CloseReason::AlreadyConnected.close(&new_conn);
                bail!(
                    "client({:?}) rejected: ipk already has a live connection",
                    hex::encode(ipk_bytes)
                );
            }
            // Stale entry — drop it, the dead task's cleanup will be a no-op
            // because the stable_id no longer matches.
            clients.remove(&ipk_bytes);
        }
        clients.insert(ipk_bytes, new_conn);
    }

    //===:===:===:===:===:===:===:===:===:===:===:===:===:===:===//

    Ok(ipk)
}
