use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::ActivityP;
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

use crate::dht::forward::ForwardSummary;
use crate::dht::forward::forward_to_homes;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::remove_client_if_same;
use crate::storage::MAX_QUEUED_PER_RECIPIENT;
use crate::storage::MessageKey;
use crate::util::systime;

const LIVE_DELIVER_ACK_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) async fn handle_forward(
    fwd: DispatchP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // 1. Sender must match the authenticated session identity. Otherwise any
    //    authenticated client could spoof messages on behalf of someone else
    //    (the signature check below would still pass for a forged `from`).
    //    This binding **stays first** — DHT fan-out can only run
    //    after we've confirmed `from == authenticated session`. (Recently-
    //    landed security fix in 1326573; see commit message for context.)
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

    // Never accept a client-provided clock. This ingress relay owns the
    // display timestamp and carries it unchanged through every later hop.
    let accepted_at_ms = systime().as_millis() as u64;
    let fwd = DispatchP { accepted_at_ms, ..fwd };

    // Snapshot the dispatch fields we need on multiple paths *without*
    // moving `fwd` yet — the K-closest path takes the whole `DispatchP`,
    // while the local-delivery / local-queue paths build a `DeliverP`
    // from its parts. Cloning is cheap relative to the network round-trip
    // we're about to make.
    let recipient: Bytes<32> = fwd.to;
    let dispatch_for_dht = DispatchP {
        to:      recipient,
        from:    fwd.from,
        id:      fwd.id,
        payload: fwd.payload.clone(),
        sig:     fwd.sig,
        accepted_at_ms,
    };
    let delivery = DeliverP {
        id:      fwd.id,
        from:    fwd.from,
        payload: fwd.payload,
        sig:     fwd.sig,
        accepted_at_ms,
    };

    // 3. Recipient online locally? Deliver-or-evict path. Online-locally
    //    short-circuits the K-closest fan-out.
    let recipient_conn = { ctx.relay.clients.read().get(&*recipient).cloned() };

    if let Some(conn) = recipient_conn {
        let delivered = try_deliver(&conn, &delivery).await;
        if delivered.is_ok() {
            SRelayPacket::DispatchAck(DispatchAckP::Delivered { accepted_at_ms }).send(tx).await?;
            return Ok(());
        }
        // The in-memory entry is dead (timed out, peer-reset, or never
        // ack'd). Evict it BEFORE the next path so a stale entry doesn't
        // make us pay another ack timeout against the corpse.
        //
        // Race-guard: only evict if the entry still points at the same
        // `Connection` we just tried — a fresh re-handshake from the
        // recipient may have already replaced it.
        remove_client_if_same(&ctx.relay, &recipient.0, &conn);
        // Fall through into the DHT/local-queue ladder.
    }

    // 4. K-closest fan-out (sticky-home). When the DHT is
    //    enabled, route the dispatch to the K-closest "home" relays for
    //    durable queueing (or remote-online delivery). On any failure
    //    mode — DHT disabled, no homes known yet, < K_MIN successes —
    //    we fall through to the local-queue safety net.
    if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        match forward_to_homes(dht, dispatch_for_dht, accepted_at_ms).await {
            Ok(summary) => {
                let ack = ack_for_summary(&summary, accepted_at_ms);
                SRelayPacket::DispatchAck(ack).send(tx).await?;
                return Ok(());
            }
            Err(err) => {
                // Fan-out couldn't reach quorum (or routing was empty).
                // Fall through to local-queue. Logging at trace because
                // a bootstrap-incomplete relay legitimately hits this.
                if let Some(metrics) = ctx.relay.dht.as_ref().map(|d| &d.metrics) {
                    metrics.inc_forward_fallbacks_to_local_queue();
                }
                trace!(
                    "FORWARD: K-closest fan-out fell back to local queue: {err}"
                );
            }
        }
    }

    // 5. Local-queue safety net. Pre-sticky-home behaviour preserved
    //    as a fallback so a transient DHT/network hiccup doesn't lose
    //    messages.
    let dispatch = store_in_rocks(&ctx, recipient, delivery)?;
    SRelayPacket::DispatchAck(dispatch).send(tx).await?;

    Ok(())
}

/// Route an ephemeral signal (presence/typing): deliver to the recipient if
/// online on THIS relay, else drop — never queue. Fire-and-forget (no reply to
/// the sender). Sender must be the authenticated session (no from-spoofing);
/// the recipient's libcore re-verifies `sig` for authenticity.
///
/// MVP: same-relay only. Cross-relay (fan to the recipient's online home) is a
/// follow-up — an offline-or-remote recipient simply doesn't see the signal.
pub(super) async fn handle_activity(eph: ActivityP, ctx: ClientCtxHandle) -> Result<()> {
    if eph.from.as_slice() != ctx.ipk.as_bytes().as_slice() {
        return Ok(());
    }
    let recipient_conn = { ctx.relay.clients.read().get(&*eph.to).cloned() };
    let Some(conn) = recipient_conn else {
        if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
            tokio::spawn(crate::dht::forward::forward_activity_to_homes(dht, eph));
        }
        return Ok(());
    };
    if let Ok((mut tx, _rx)) = conn.open_bi().await {
        let _ = SRelayPacket::Activity(eph).send(&mut tx).await;
        let _ = tx.finish();
    }
    Ok(())
}

/// Translate a successful [`ForwardSummary`] into the [`DispatchAckP`]
/// variant the originating client expects:
///
/// - Any home returned `Delivered` → [`DispatchAckP::Delivered`].
/// - Otherwise (≥ K_MIN homes returned `Stored`) →
///   [`DispatchAckP::Forwarded`].
///
/// Pure function so it can be unit-tested without spinning up a network.
fn ack_for_summary(summary: &ForwardSummary, accepted_at_ms: u64) -> DispatchAckP {
    if summary.any_delivered() {
        DispatchAckP::Delivered { accepted_at_ms }
    } else {
        DispatchAckP::Forwarded { accepted_at_ms }
    }
}

/// Attempt direct delivery. All failure modes (open_bi, send, ack timeout,
/// wrong-packet) collapse into `Err(ConnectionError::TimedOut)` because the
/// caller only needs to distinguish success from "give up and queue".
///
/// Exposed at `pub(crate)` and accepting only `(conn, delivery)` so
/// the home-side `Forward` RPC handler in
/// [`crate::dht::forward::handle_forward_rpc`] can reuse the exact same
/// deliver-then-ack protocol when the recipient is online here. Keeping
/// one implementation across the sender-side and home-side delivery
/// paths means a future tweak to the ack window lands
/// in one place and stays consistent.
pub(crate) async fn try_deliver(
    conn: &Connection, delivery: &DeliverP,
) -> Result<(), ConnectionError> {
    let (mut deliver_tx, mut deliver_rx) = conn.open_bi().await?;

    SRelayPacket::Deliver(delivery.clone())
        .send(&mut deliver_tx)
        .await
        .map_err(|_| ConnectionError::TimedOut)?;

    match tokio::time::timeout(LIVE_DELIVER_ACK_TIMEOUT, CRelayPacket::unpack(&mut deliver_rx)).await {
        Ok(Ok(CRelayPacket::DeliverAck)) => Ok(()),
        _ => Err(ConnectionError::TimedOut),
    }
}

/// Build a [`DeliverP`] from a [`DispatchP`]. Strips the recipient
/// (`to`) field — `DeliverP` is the recipient's view, where the
/// recipient is implicit. Used by the home-side `Forward` handler in
/// [`crate::dht::forward::handle_forward_rpc`] to convert an inbound
/// dispatch into the on-the-wire delivery shape before calling
/// [`try_deliver`].
///
/// Mirrors the field-by-field shape used in
/// `events/drain.rs::dispatch_to_deliver`; the duplication is
/// intentional — both modules are end-points of the dispatch ladder
/// and a shared util would only export one extra symbol without
/// reducing the per-callsite line count.
pub(crate) fn dispatch_to_deliver(d: &DispatchP) -> DeliverP {
    DeliverP {
        id:      d.id,
        from:    d.from,
        payload: d.payload.clone(),
        sig:     d.sig,
        accepted_at_ms: d.accepted_at_ms,
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

    // Per-recipient cap (Part B3). fjall's `prefix()` is an exact scan, so
    // we just count the recipient's keys. Bounded: stop as soon as we hit
    // `MAX + 1` so we don't walk a million-entry queue on every dispatch.
    let mut count: usize = 0;
    let stop_at = MAX_QUEUED_PER_RECIPIENT.saturating_add(1);
    for guard in ctx.relay.store.messages.prefix(recipient.0) {
        // Treat a corrupted iterator as "we can't be sure we're under the
        // cap" — better to reject than silently overrun.
        if guard.key().is_err() {
            return Ok(DispatchAckP::Error { reason: "queue scan failed".into() });
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

    let key = MessageKey::new(&recipient.0, delivery.accepted_at_ms, &delivery.id.0);

    // Durable write: we acknowledge `Queued` to the sender as soon as this
    // returns, so a crash before the write hits disk would silently lose the
    // message. `put_sync` fsyncs the journal before we ack.
    let payload = delivery.ser()?;
    ctx.relay.store.put_sync(&ctx.relay.store.messages, key.as_bytes(), &payload)?;

    Ok(DispatchAckP::Queued { accepted_at_ms: delivery.accepted_at_ms })
}

#[cfg(test)]
mod tests {
    use common::quic::id::NodeId;

    use super::ack_for_summary;
    use crate::dht::forward::ForwardSummary;
    use common::proto::client_rel::DispatchAckP;

    fn id_for(n: u8) -> NodeId {
        let mut b = [0u8; 32];
        b[0] = n;
        NodeId::new(b)
    }

    /// `any_delivered = true` always wins, even when there are also
    /// `stored_at` entries — `Delivered` is the strictly stronger promise.
    #[test]
    fn ack_for_summary_promotes_to_delivered_when_any_home_delivered() {
        let mut s = ForwardSummary::default();
        s.delivered_at.push(id_for(1));
        s.stored_at.push(id_for(2));
        match ack_for_summary(&s, 1) {
            DispatchAckP::Delivered { accepted_at_ms: 1 } => {}
            other => panic!("expected Delivered, got {other:?}"),
        }
    }

    /// All-stored homes → `Forwarded`. `Forwarded` is distinct from
    /// `Queued`, which is the local-only fallback path.
    #[test]
    fn ack_for_summary_returns_forwarded_when_only_stored() {
        let mut s = ForwardSummary::default();
        s.stored_at.push(id_for(1));
        s.stored_at.push(id_for(2));
        match ack_for_summary(&s, 1) {
            DispatchAckP::Forwarded { accepted_at_ms: 1 } => {}
            other => panic!("expected Forwarded, got {other:?}"),
        }
    }
}
