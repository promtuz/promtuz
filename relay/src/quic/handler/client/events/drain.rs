use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::SRelayPacket;
use common::proto::pack::Unpacker;
use common::trace;
use common::warn;
use quinn::SendStream;
use rust_rocksdb::WriteBatch;

use crate::quic::handler::client::ClientCtxHandle;
use crate::storage::MessageKey;

/// Sends all pending messages to the user. The queue is *not* cleared yet —
/// the client must follow up with `AckDrain` (handled by [`handle_ack_drain`])
/// once it has durably stored everything.
///
/// If the client triggers another `DrainQueue` before acking, we re-send the
/// previously-tracked set plus any newly arrived messages. We do not reset
/// the tracked-key list until the ack arrives.
pub(super) async fn handle_drain_queue(
    ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    let recipient = ctx.ipk.as_bytes();

    // `prefix_iterator` is a *seek hint* — RocksDB will happily walk past our
    // recipient prefix into the next user's queue. We must explicitly filter
    // every key by the 32-byte recipient or we'd leak other recipients'
    // messages on the wire.
    let queue = ctx.relay.rocks.prefix_iterator(recipient);

    let mut delivered_keys: Vec<MessageKey> = Vec::new();

    for entry in queue {
        let (key_bytes, value) = match entry {
            Ok(kv) => kv,
            Err(e) => {
                warn!("DRAIN: rocks iterator error: {e}");
                break;
            },
        };

        if !key_bytes.starts_with(recipient) {
            // Walked past our prefix; we're done.
            break;
        }

        let Some(key) = MessageKey::parse(&key_bytes) else {
            warn!("DRAIN: malformed key (len={}); skipping", key_bytes.len());
            continue;
        };

        let Ok(deliver) = DeliverP::deser(&value) else {
            warn!("DRAIN: malformed DeliverP value; skipping");
            continue;
        };

        // Don't log the payload — it's encrypted ciphertext, but logging it
        // still exposes per-message size+sig metadata to whoever can read
        // operator logs. Just record that we sent something.
        trace!("DRAIN: sending queued message id={}", hex::encode(deliver.id));

        SRelayPacket::Deliver(deliver).send(tx).await?;
        delivered_keys.push(key);
    }

    // Replace (rather than extend) so that a re-drain before ack still
    // captures the live set. The previous batch is naturally a subset of
    // what's still on disk (we haven't deleted yet), so we'd otherwise grow
    // the pending list with duplicates.
    *ctx.pending_drain.lock() = delivered_keys;

    Ok(())
}

/// Atomically deletes every key the most recent drain delivered.
pub(super) async fn handle_ack_drain(
    ctx: ClientCtxHandle, _tx: &mut SendStream,
) -> Result<()> {
    let keys = std::mem::take(&mut *ctx.pending_drain.lock());

    if keys.is_empty() {
        return Ok(());
    }

    let mut batch = WriteBatch::default();
    for key in &keys {
        batch.delete(key.as_bytes());
    }

    ctx.relay.rocks.write(&batch)?;

    trace!("DRAIN: cleared {} acked messages", keys.len());

    Ok(())
}
