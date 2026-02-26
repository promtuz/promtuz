use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::proto::client_rel::HandshakeP;
use common::proto::client_rel::RelayPacket;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::util::systime;

pub(super) async fn handle_handshake(
    packet: HandshakeP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    use HandshakeP::*;
    use RelayPacket::*;

    let mut ctx = ctx.write().await;

    match packet {
        ClientHello { ipk } => {
            ctx.ipk = Some(VerifyingKey::from_bytes(&ipk)?);
            Handshake(ServerChallenge { nonce: ctx.nonce }).send(tx).await?;
        },
        ClientProof { sig } => {
            let ipk = ctx.ipk.ok_or(anyhow!("no ipk yet"))?;
            let ipk_bytes = ipk.to_bytes();

            let msg =
                [b"relay-auth-v" as &[u8], &PROTOCOL_VERSION.to_be_bytes(), &ctx.nonce].concat();

            let packet = match Signature::from_slice(&sig) {
                Ok(sig) if ipk.verify_strict(&msg, &sig).is_ok() => {
                    ServerAccept { timestamp: systime().as_secs() }
                },
                _ => ServerReject { reason: "Invalid Signature".into() },
            };

            Handshake(packet).send(tx).await?;
            _ = tx.finish();

            // Register this client as connected
            {
                let relay_ref = ctx.relay.clone();
                let conn = ctx.conn.clone();
                let mut relay = relay_ref.lock().await;
                relay.clients.insert(ipk_bytes, conn);
            }
        },
        _ => return Err(anyhow!("No!")),
    }

    Ok(())
}
