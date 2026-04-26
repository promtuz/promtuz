use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::trace;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use quinn::ConnectionError;
use quinn::SendStream;
use rust_rocksdb::WriteOptions;

use crate::quic::handler::client::ClientCtxHandle;
use crate::storage::MessageKey;
use crate::util::systime;

pub(super) async fn handle_forward(
    fwd: DispatchP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // 1. Sender must match the authenticated session identity. Otherwise any
    //    authenticated client could spoof messages on behalf of someone else
    //    (the signature check below would still pass for a forged `from`).
    if fwd.from.as_slice() != ctx.ipk.as_bytes().as_slice() {
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig).send(tx).await?;
        return Ok(());
    }

    // 2. Verify signature: sender must prove authorship under the canonical
    //    domain-separated, version-tagged, id-bound construction.
    let sig_valid = (|| {
        let vk = VerifyingKey::from_bytes(&fwd.from).ok()?;
        let sig = Signature::from_slice(&*fwd.sig).ok()?;
        let msg = dispatch_sig_message(&fwd.to, &fwd.from, &fwd.id, &fwd.payload);
        vk.verify_strict(&msg, &sig).ok()
    })();

    if sig_valid.is_none() {
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig).send(tx).await?;
        return Ok(());
    }

    let (recipient, delivery) = {
        let DispatchP { to, from, id, payload, sig } = fwd;
        (to, DeliverP { id, from, payload, sig })
    };

    // 3. Check if recipient is connected locally
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

    let ts_ms = systime().as_millis() as u64;
    let key = MessageKey::new(&recipient.0, ts_ms, &delivery.id.0);

    // Durable write: we acknowledge `Queued` to the sender as soon as this
    // returns, so a crash before the WAL hits disk would silently lose the
    // message. `set_sync(true)` guarantees fsync of the WAL before ack.
    let mut wopts = WriteOptions::default();
    wopts.set_sync(true);

    ctx.relay.rocks.put_opt(key.as_bytes(), delivery.ser()?, &wopts)?;

    Ok(())
}
