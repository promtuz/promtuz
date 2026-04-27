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
use quinn::Connection;
use quinn::ConnectionError;
use quinn::SendStream;
use rust_rocksdb::WriteOptions;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::remove_client_if_same;
use crate::storage::MAX_QUEUED_PER_RECIPIENT;
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
        let delivered = try_deliver(&conn, &delivery).await;

        if delivered.is_err() {
            // The in-memory entry is dead (timed out, peer-reset, or never
            // ack'd). Evict it BEFORE queuing so the next dispatch for this
            // recipient skips straight to the queue path instead of paying
            // another 3s timeout against the corpse.
            //
            // Race-guard: only evict if the entry still points at the same
            // `Connection` we just tried — a fresh re-handshake from the
            // recipient may have already replaced it.
            remove_client_if_same(&ctx.relay, &recipient.0, &conn);

            store_in_rocks(&ctx, recipient, delivery)?
        } else {
            DispatchAckP::Delivered
        }
    } else {
        store_in_rocks(&ctx, recipient, delivery)?
    };

    SRelayPacket::DispatchAck(dispatch).send(tx).await?;

    Ok(())
}

/// Attempt direct delivery. All failure modes (open_bi, send, ack timeout,
/// wrong-packet) collapse into `Err(ConnectionError::TimedOut)` because the
/// caller only needs to distinguish success from "give up and queue".
async fn try_deliver(conn: &Connection, delivery: &DeliverP) -> Result<(), ConnectionError> {
    let (mut deliver_tx, mut deliver_rx) = conn.open_bi().await?;

    SRelayPacket::Deliver(delivery.clone())
        .send(&mut deliver_tx)
        .await
        .map_err(|_| ConnectionError::TimedOut)?;

    match tokio::time::timeout(Duration::from_secs(3), CRelayPacket::unpack(&mut deliver_rx)).await
    {
        Ok(Ok(CRelayPacket::DeliverAck)) => Ok(()),
        _ => Err(ConnectionError::TimedOut),
    }
}

/// Attempt to durably queue `delivery`. Returns the appropriate
/// `DispatchAckP` for the sender:
/// - `Queued` on success
/// - `QueueFull` if the recipient already has `MAX_QUEUED_PER_RECIPIENT`
///   messages on disk; the message is *not* stored in this case.
fn store_in_rocks(
    ctx: &ClientCtxHandle, recipient: Bytes<32>, delivery: DeliverP,
) -> Result<DispatchAckP> {
    trace!("FORWARD: recipient {} not connected locally, queuing", hex::encode(recipient));

    // Per-recipient cap (Part B3). We must count keys with this exact
    // recipient prefix; `prefix_iterator` is just a seek hint and may walk
    // into the next user's keyspace, so the same `starts_with` filter the
    // drain path uses applies here too.
    //
    // Bounded count: stop as soon as we hit `MAX + 1` so we don't walk a
    // million-entry queue on every dispatch.
    let mut count: usize = 0;
    let stop_at = MAX_QUEUED_PER_RECIPIENT.saturating_add(1);
    for entry in ctx.relay.rocks.prefix_iterator(&recipient.0) {
        let (key_bytes, _) = match entry {
            Ok(kv) => kv,
            // Treat a corrupted iterator as "we can't be sure we're under
            // the cap" — better to reject than silently overrun.
            Err(_) => return Ok(DispatchAckP::Error { reason: "queue scan failed".into() }),
        };
        if !key_bytes.starts_with(&recipient.0) {
            // Walked past our prefix; we're done.
            break;
        }
        count += 1;
        if count >= stop_at {
            break;
        }
    }
    if count >= MAX_QUEUED_PER_RECIPIENT {
        trace!(
            "FORWARD: queue full for recipient {} ({} >= {}); rejecting",
            hex::encode(recipient),
            count,
            MAX_QUEUED_PER_RECIPIENT
        );
        return Ok(DispatchAckP::QueueFull);
    }

    let ts_ms = systime().as_millis() as u64;
    let key = MessageKey::new(&recipient.0, ts_ms, &delivery.id.0);

    // Durable write: we acknowledge `Queued` to the sender as soon as this
    // returns, so a crash before the WAL hits disk would silently lose the
    // message. `set_sync(true)` guarantees fsync of the WAL before ack.
    let mut wopts = WriteOptions::default();
    wopts.set_sync(true);

    ctx.relay.rocks.put_opt(key.as_bytes(), delivery.ser()?, &wopts)?;

    Ok(DispatchAckP::Queued)
}
