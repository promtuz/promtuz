use std::time::Duration;

use anyhow::Result;
use common::crypto::get_nonce;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::trace;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use quinn::ConnectionError;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::util::systime;

pub fn uuid() -> Bytes<16> {
    Bytes(uuid::Uuid::now_v7().into_bytes())
}

pub(super) async fn handle_forward(
    fwd: DispatchP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // 1. Verify signature: sender must prove authorship
    let sig_valid = (|| {
        let vk = VerifyingKey::from_bytes(&fwd.from).ok()?;
        let sig = Signature::from_slice(&*fwd.sig).ok()?;
        let msg = [fwd.to.as_slice(), fwd.from.as_slice(), &fwd.payload].concat();
        vk.verify_strict(&msg, &sig).ok()
    })();

    if sig_valid.is_none() {
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig).send(tx).await?;
        return Ok(());
    }

    let (recipient, delivery) = {
        let DispatchP { to, from, payload, sig } = fwd;
        (to, DeliverP { id: uuid(), from, payload, sig })
    };

    // 2. Check if recipient is connected locally
    let recipient_conn = { ctx.relay.clients.read().get(&*recipient).cloned() };

    let dispatch = if let Some(conn) = recipient_conn {
        let delivered = async {
            let (mut deliver_tx, mut deliver_rx) = conn.open_bi().await?;

            SRelayPacket::Deliver(delivery.clone())
                .send(&mut deliver_tx)
                .await
                .map_err(|_| ConnectionError::TimedOut)?;

            match tokio::time::timeout(
                Duration::from_secs(3),
                CRelayPacket::unpack(&mut deliver_rx),
            )
            .await
            {
                Ok(Ok(CRelayPacket::DeliverAck)) => Ok(()),
                _ => Err(ConnectionError::TimedOut),
            }
        }
        .await;

        if delivered.is_err() {
            // Connection was stale — store for later pickup
            store_in_rocks(&ctx, recipient, delivery)?;

            DispatchAckP::Queued
        } else {
            DispatchAckP::Delivered
        }
    } else {
        store_in_rocks(&ctx, recipient, delivery)?;

        DispatchAckP::Queued
    };

    SRelayPacket::DispatchAck(dispatch).send(tx).await?;

    Ok(())
}

fn store_in_rocks(ctx: &ClientCtxHandle, recipient: Bytes<32>, delivery: DeliverP) -> Result<()> {
    trace!("FORWARD: recipient {} not connected locally, queuing", hex::encode(recipient));

    let timestamp: [u8; 8] = (systime().as_millis() as u64).to_be_bytes();
    let rand = get_nonce::<4>();

    let mut key = [0u8; 44];
    key[..32].copy_from_slice(&*recipient);
    key[32..40].copy_from_slice(&timestamp);
    key[40..44].copy_from_slice(&rand);

    ctx.relay.rocks.put(key, delivery.ser()?)?;

    Ok(())
}
