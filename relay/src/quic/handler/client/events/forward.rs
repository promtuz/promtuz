use std::time::Duration;

use anyhow::Result;
use common::proto::Sender;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::ActivityP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::activity_sig_message;
use common::proto::client_rel::dispatch_sig_message;
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::debug;
use common::trace;
use common::types::bytes::Bytes;
use ed25519_dalek::Signature;
use ed25519_dalek::VerifyingKey;
use quinn::Connection;
use quinn::ConnectionError;
use quinn::SendStream;

use crate::dht::forward::ForwardSummary;
use crate::dht::forward::forward_to_homes;
use crate::dht::store::QueueAdmission;
use crate::dht::store::admit_to_queue;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::STREAM_OPEN_TIMEOUT;
use crate::quic::handler::client::events::spawn_tied;
use crate::quic::handler::client::remove_client_if_same;
use crate::storage::MessageKey;
use crate::util::systime;

const LIVE_DELIVER_ACK_TIMEOUT: Duration = Duration::from_secs(15);
/// Matches the far-end window in `dht::forward::handle_activity_forward_rpc`.
const ACTIVITY_MAX_SKEW_MS: u64 = 30_000;

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
        wake:    fwd.wake,
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
            debug!(
                "dispatch {}: delivered live to {} (recipient online here)",
                hex::encode(&delivery.id.0[..8]),
                hex::encode(&recipient.0[..8])
            );
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
    let dispatch = store_in_rocks(&ctx, recipient, delivery).await?;
    SRelayPacket::DispatchAck(dispatch).send(tx).await?;

    Ok(())
}

/// Route an ephemeral signal (presence/typing): deliver to the recipient if
/// online on THIS relay, else fan out to its homes — never queue.
/// Fire-and-forget (no reply to the sender). Sender must be the authenticated
/// session and the signal must carry a fresh, valid signature; a K-way fan-out
/// is far too expensive to spend on bytes we have not authenticated.
pub(super) async fn handle_activity(eph: ActivityP, ctx: ClientCtxHandle) -> Result<()> {
    if eph.from.as_slice() != ctx.ipk.as_bytes().as_slice() {
        return Ok(());
    }
    if !activity_is_authentic(&eph, systime().as_millis() as u64) {
        return Ok(());
    }
    let recipient_conn = { ctx.relay.clients.read().get(&*eph.to).cloned() };
    let Some(conn) = recipient_conn else {
        if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
            spawn_tied(&ctx.cancel, crate::dht::forward::forward_activity_to_homes(dht, eph));
        }
        return Ok(());
    };
    let _ = tokio::time::timeout(STREAM_OPEN_TIMEOUT, async {
        let (mut tx, _rx) = conn.open_bi().await.ok()?;
        SRelayPacket::Activity(eph).send(&mut tx).await.ok()?;
        tx.finish().ok()
    })
    .await;
    Ok(())
}

fn activity_is_authentic(eph: &ActivityP, now_ms: u64) -> bool {
    if now_ms.abs_diff(eph.timestamp) > ACTIVITY_MAX_SKEW_MS {
        return false;
    }
    (|| {
        let vk = VerifyingKey::from_bytes(&eph.from).ok()?;
        let sig = Signature::from_slice(&*eph.sig).ok()?;
        let msg =
            activity_sig_message(&eph.to, &eph.from, &eph.conversation, eph.activity, eph.timestamp);
        vk.verify_strict(&msg, &sig).ok()
    })()
    .is_some()
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
    let (mut deliver_tx, mut deliver_rx) =
        match tokio::time::timeout(STREAM_OPEN_TIMEOUT, conn.open_bi()).await {
            Ok(opened) => opened?,
            Err(_) => return Err(ConnectionError::TimedOut),
        };

    match tokio::time::timeout(
        LIVE_DELIVER_ACK_TIMEOUT,
        SRelayPacket::Deliver(delivery.clone()).send(&mut deliver_tx),
    )
    .await
    {
        Ok(Ok(())) => {},
        _ => return Err(ConnectionError::TimedOut),
    }

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
async fn store_in_rocks(
    ctx: &ClientCtxHandle, recipient: Bytes<32>, delivery: DeliverP,
) -> Result<DispatchAckP> {
    debug!(
        "dispatch {}: recipient {} offline — queued locally (fallback)",
        hex::encode(&delivery.id.0[..8]),
        hex::encode(&recipient.0[..8])
    );

    match admit_to_queue(&ctx.relay.store.messages, &recipient.0, &delivery.id.0, &delivery.from.0, |v| {
        DeliverP::deser(v).ok().map(|d| d.from.0)
    }) {
        QueueAdmission::Insert => {},
        QueueAdmission::AlreadyQueued => {
            return Ok(DispatchAckP::Queued { accepted_at_ms: delivery.accepted_at_ms });
        },
        QueueAdmission::IdTakenByOther => {
            return Ok(DispatchAckP::Error { reason: "dispatch id already queued".into() });
        },
        QueueAdmission::ScanFailed => {
            return Ok(DispatchAckP::Error { reason: "queue scan failed".into() });
        },
        QueueAdmission::Full => {
            trace!("FORWARD: queue full for recipient {}; rejecting", hex::encode(recipient));
            return Ok(DispatchAckP::QueueFull);
        },
    }

    let key = MessageKey::new(&recipient.0, delivery.accepted_at_ms, &delivery.id.0);

    // `Queued` is a durability promise, so the write must be on disk before we
    // reply — the barrier resolves on the group commit covering it.
    let payload = delivery.ser()?;
    ctx.relay.store.put_sync(&ctx.relay.store.messages, key.as_bytes(), &payload)?;
    ctx.relay.store.persist_barrier().wait().await?;

    Ok(DispatchAckP::Queued { accepted_at_ms: delivery.accepted_at_ms })
}

#[cfg(test)]
mod tests {
    use common::proto::client_rel::ActivityP;
    use common::proto::client_rel::activity_sig_message;
    use common::quic::id::NodeId;
    use ed25519_dalek::Signer;
    use ed25519_dalek::SigningKey;

    use super::ACTIVITY_MAX_SKEW_MS;
    use super::ack_for_summary;
    use super::activity_is_authentic;
    use crate::dht::forward::ForwardSummary;
    use common::proto::client_rel::DispatchAckP;

    fn id_for(n: u8) -> NodeId {
        let mut b = [0u8; 32];
        b[0] = n;
        NodeId::new(b)
    }

    fn signed_activity(key: &SigningKey, timestamp: u64) -> ActivityP {
        let to = [9u8; 32];
        let from = key.verifying_key().to_bytes();
        let activity = 1u16;
        let conversation = [4u8; 16];
        let sig = key
            .sign(&activity_sig_message(&to, &from, &conversation, activity, timestamp))
            .to_bytes();
        ActivityP {
            to: to.into(),
            from: from.into(),
            conversation: conversation.into(),
            activity,
            timestamp,
            sig: sig.into(),
        }
    }

    #[test]
    fn activity_is_authentic_accepts_a_fresh_signed_signal() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let now = 1_700_000_000_000u64;
        assert!(activity_is_authentic(&signed_activity(&key, now), now));
    }

    #[test]
    fn activity_is_authentic_rejects_a_forged_signature() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let now = 1_700_000_000_000u64;
        let mut eph = signed_activity(&key, now);
        eph.sig = [0u8; 64].into();
        assert!(!activity_is_authentic(&eph, now));
    }

    #[test]
    fn activity_is_authentic_rejects_a_replayed_timestamp() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let now = 1_700_000_000_000u64;
        let stale = now - ACTIVITY_MAX_SKEW_MS - 1;
        assert!(!activity_is_authentic(&signed_activity(&key, stale), now));
    }

    #[test]
    fn activity_is_authentic_rejects_a_signal_retargeted_at_another_recipient() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let now = 1_700_000_000_000u64;
        let mut eph = signed_activity(&key, now);
        eph.to = [8u8; 32].into();
        assert!(!activity_is_authentic(&eph, now));
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
