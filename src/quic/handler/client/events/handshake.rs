use anyhow::Result;
use anyhow::anyhow;
use common::PROTOCOL_VERSION;
use common::msg::relay::HandshakeP;
use common::msg::relay::RelayPacket;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;

use crate::dht::UserMetadata;
use crate::dht::UserRecord;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::peer::replicate_user;
use crate::util::systime;

pub(super) async fn handle_handshake(packet: HandshakeP, ctx: ClientCtxHandle) -> Result<()> {
    use HandshakeP::*;
    use RelayPacket::*;

    let mut ctx = ctx.write().await;

    match packet {
        ClientHello { ipk } => {
            println!("CLIENT HELLO: {:?}", ipk);
            ctx.ipk = Some(VerifyingKey::from_bytes(&ipk)?);
            Handshake(ServerChallenge { nonce: ctx.nonce })
                .send(ctx.send.as_mut().unwrap())
                .await?;
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

            Handshake(packet).send(ctx.send.as_mut().unwrap()).await?;
            _ = ctx.send.as_mut().unwrap().finish();

            let relay_ref = ctx.relay.clone();
            tokio::spawn(async move {
                let record = {
                    let relay_guard = relay_ref.lock().await;
                    UserRecord {
                        ipk: ipk_bytes,
                        relay: relay_guard.id,
                        // FIXME: relay_addr is currently storing local address, which is no use
                        relay_addr: relay_guard.cfg.network.address,
                        timestamp: systime().as_secs(),
                        signature: None,
                        metadata: UserMetadata {
                            status: Some("online".into()),
                            // capabilities: vec![],
                        },
                    }
                };
                let dht = { relay_ref.lock().await.dht.clone() };
                {
                    let mut dht = dht.write().await;
                    dht.upsert_user(record.clone());
                }
                replicate_user(relay_ref.clone(), record).await;
            });
        },
        _ => return Err(anyhow!("No!")),
    }

    Ok(())
}
