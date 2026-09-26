use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use common::debug;
use common::proto::Sender;
use common::proto::client_rel::ActivityP;
use common::proto::client_rel::CRelayPacket;
use common::proto::client_rel::DeliverP;
use common::proto::client_rel::DispatchAckP;
use common::proto::client_rel::DispatchP;
use common::proto::client_rel::SRelayPacket;
use common::proto::client_rel::activity_sig_message;
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
use crate::dht::store::QueueAdmission;
use crate::dht::store::admit_to_queue;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::STREAM_OPEN_TIMEOUT;
use crate::quic::handler::client::events::spawn_tied;
use crate::quic::handler::client::remove_client_if_same;
use crate::storage::MessageKey;
use crate::storage::db::Store;
use crate::util::systime;

/// Bounds work retained after accepting a dispatch, including remote routing.
const ROUTE_TIMEOUT: Duration = Duration::from_secs(30);

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
    // Over budget: refused before the signature is even checked, with the
    // same answer a full queue gives — the client backs off and retries from
    // its outbox, which is exactly right for both.
    if ctx.limits.dispatch.check().is_err() {
        SRelayPacket::DispatchAck(DispatchAckP::QueueFull).send(tx).await?;
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

    let recipient = fwd.to;
    let delivery = dispatch_to_deliver(&fwd);
    accept_dispatch(
        store_in_rocks(&ctx.relay.store, recipient, &delivery),
        route_dispatch(fwd.clone(), &ctx),
        |ack| async move { SRelayPacket::DispatchAck(ack).send(tx).await.map_err(Into::into) },
        || retire_local_copy(&ctx.relay.store, &fwd),
    )
    .await
}

/// A durable local copy is enough to release the sender. Keep routing after
/// replying so a slow/stale recipient never gates acceptance. This stays in
/// the packet task (and its concurrency permit), with a total routing budget.
/// A lost sender ack also must not cancel delivery of an accepted message.
async fn accept_dispatch<Q, R, S, SF, C>(queue: Q, route: R, respond: S, retire: C) -> Result<()>
where
    Q: Future<Output = Result<DispatchAckP>>,
    R: Future<Output = Option<DispatchAckP>>,
    S: FnOnce(DispatchAckP) -> SF,
    SF: Future<Output = Result<()>>,
    C: FnOnce() -> Result<()>,
{
    let queued = queue.await?;
    match queued {
        DispatchAckP::Queued { .. } => {
            let reply = respond(queued).await;
            if let Ok(Some(DispatchAckP::Delivered { .. } | DispatchAckP::Forwarded { .. })) =
                tokio::time::timeout(ROUTE_TIMEOUT, route).await
            {
                // A recipient ack or durable home quorum now owns the copy.
                // Failed cleanup merely leaves a deduplicated retry for drain.
                if let Err(err) = retire() {
                    trace!("FORWARD: retained local copy after successful routing: {err}");
                }
            }
            reply
        },
        DispatchAckP::QueueFull => {
            // A full local queue need not prevent live delivery or home storage.
            let ack =
                tokio::time::timeout(ROUTE_TIMEOUT, route).await.ok().flatten().unwrap_or(queued);
            respond(ack).await
        },
        // A collision or failed admission must not be routed around.
        _ => respond(queued).await,
    }
}

/// Deliver locally or transfer custody to the recipient's homes. `None`
/// leaves the already-accepted local copy available for a later drain.
async fn route_dispatch(fwd: DispatchP, ctx: &ClientCtxHandle) -> Option<DispatchAckP> {
    let accepted_at_ms = fwd.accepted_at_ms;
    let recipient = fwd.to;
    let delivery = dispatch_to_deliver(&fwd);
    // A recipient connected here can take custody without a home fan-out.
    let recipient_conn = { ctx.relay.clients.read().get(&*recipient).cloned() };

    if let Some(conn) = recipient_conn {
        let delivered = try_deliver(&conn, &delivery).await;
        if delivered.is_ok() {
            debug!(
                "dispatch {}: delivered live to {} (recipient online here)",
                hex::encode(&delivery.id.0[..8]),
                hex::encode(&recipient.0[..8])
            );
            return Some(DispatchAckP::Delivered { accepted_at_ms });
        }
        // Evict only a connection that is actually gone. A live one that
        // did not ack — the recipient dropped the envelope, or is slow —
        // stays on the map: evicting on a bare timeout let any stranger
        // knock a user offline with one junk dispatch, since the entry only
        // comes back on a fresh handshake.
        //
        // Race-guard: only evict if the entry still points at the same
        // `Connection` we just tried — a fresh re-handshake from the
        // recipient may have already replaced it.
        if conn.close_reason().is_some() {
            remove_client_if_same(&ctx.relay, &recipient.0, &conn);
        }
        // Try the homes next; the local copy remains until custody transfers.
    }

    // Transfer custody to the K-closest homes for durable queueing or remote
    // delivery. An unavailable DHT or insufficient quorum leaves the local copy.
    if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        match forward_to_homes(dht, fwd, accepted_at_ms).await {
            Ok(summary) => {
                return Some(ack_for_summary(&summary, accepted_at_ms));
            }
            Err(err) => {
                // Fan-out couldn't reach quorum (or routing was empty).
                // Retain the local queue copy. Logging at trace because
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

    None
}

/// Remove only copies of this exact sender-signed dispatch. Retried dispatches
/// can have a different ingress timestamp, so the current key alone is not
/// sufficient. Other senders' colliding IDs must never be deleted.
fn retire_local_copy(store: &Store, fwd: &DispatchP) -> Result<()> {
    let mut batch = store.batch();
    for entry in store.messages.prefix(fwd.to.0) {
        let (key, value) = entry.into_inner()?;
        let Some(key_fields) = MessageKey::parse(&key) else { continue };
        if key_fields.id == fwd.id.0
            && let Ok(queued) = DeliverP::deser_compat(&value)
            && queued.from == fwd.from
            && queued.payload == fwd.payload
            && queued.sig == fwd.sig
        {
            batch.remove(&store.messages, key);
        }
    }
    batch.commit()?;
    Ok(())
}

/// Route an ephemeral signal (presence/typing): deliver to the recipient if
/// online on THIS relay, else fan out to its homes — never queue.
/// Fire-and-forget (no reply to the sender). Sender must be the authenticated
/// session and the signal must carry a fresh, valid signature; a K-way fan-out
/// is far too expensive to spend on bytes we have not authenticated.
pub(super) async fn handle_activity(eph: ActivityP, ctx: ClientCtxHandle) -> Result<()> {
    if ctx.limits.dispatch.check().is_err() {
        return Ok(());
    }
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
            activity_sig_message(&eph.to, &eph.from, &eph.group_id, eph.activity, eph.timestamp);
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
        ttl_ms:  d.ttl_ms,
        wake:    d.wake,
    }
}

/// Attempt to durably queue `delivery`. Returns the appropriate
/// `DispatchAckP` for the sender:
/// - `Queued` on success
/// - `QueueFull` if the recipient already has `MAX_QUEUED_PER_RECIPIENT`
///   messages on disk; the message is *not* stored in this case.
async fn store_in_rocks(
    store: &Store, recipient: Bytes<32>, delivery: &DeliverP,
) -> Result<DispatchAckP> {
    debug!(
        "dispatch {}: recipient {} — accepting in local queue",
        hex::encode(&delivery.id.0[..8]),
        hex::encode(&recipient.0[..8])
    );

    match admit_to_queue(&store.messages, &recipient.0, &delivery.id.0, &delivery.from.0, |v| {
        DeliverP::deser_compat(v).ok().map(|d| d.from.0)
    }) {
        QueueAdmission::Insert => {},
        QueueAdmission::AlreadyQueued => {
            store.persist_barrier().wait().await?;
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
    store.put_sync(&store.messages, key.as_bytes(), &payload)?;
    store.persist_barrier().wait().await?;

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
        let group_id = [4u8; 32];
        let sig = key
            .sign(&activity_sig_message(&to, &from, &group_id, activity, timestamp))
            .to_bytes();
        ActivityP {
            to: to.into(),
            from: from.into(),
            group_id: group_id.into(),
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

    #[test]
    fn activity_cannot_be_retargeted_at_another_group() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let now = 1_700_000_000_000u64;
        let mut eph = signed_activity(&key, now);
        eph.group_id = [8u8; 32].into();
        assert!(!activity_is_authentic(&eph, now));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_recipient_does_not_delay_acceptance_and_keeps_fallback() {
        use std::cell::Cell;
        use std::time::Duration;
        let start = tokio::time::Instant::now();
        let durable = Cell::new(false);
        let replied = Cell::new(false);
        super::accept_dispatch(
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                durable.set(true);
                Ok(DispatchAckP::Queued { accepted_at_ms: 42 })
            },
            async {
                assert!(replied.get(), "recipient contact starts after sender acceptance");
                std::future::pending().await
            },
            |ack| {
                assert!(matches!(ack, DispatchAckP::Queued { accepted_at_ms: 42 }));
                async {
                    assert!(durable.get(), "never acknowledge before the persistence barrier");
                    assert_eq!(start.elapsed(), Duration::from_millis(10));
                    replied.set(true);
                    Ok(())
                }
            },
            || panic!("a timed-out route must retain the durable fallback"),
        )
        .await
        .unwrap();
        assert!(replied.get());
        assert_eq!(start.elapsed(), Duration::from_millis(10) + super::ROUTE_TIMEOUT);
    }

    #[tokio::test]
    async fn successful_routing_retires_fallback_even_if_sender_disconnects() {
        use std::cell::Cell;
        for ack in [
            DispatchAckP::Delivered { accepted_at_ms: 42 },
            DispatchAckP::Forwarded { accepted_at_ms: 42 },
        ] {
            let retired = Cell::new(false);
            let result = super::accept_dispatch(
                async { Ok(DispatchAckP::Queued { accepted_at_ms: 42 }) },
                async { Some(ack) },
                |_| async { anyhow::bail!("sender disconnected") },
                || {
                    retired.set(true);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
            assert!(retired.get());
        }
    }

    #[tokio::test]
    async fn failed_route_keeps_accepted_copy() {
        super::accept_dispatch(
            async { Ok(DispatchAckP::Queued { accepted_at_ms: 42 }) },
            async { None },
            |ack| async move {
                assert!(matches!(ack, DispatchAckP::Queued { .. }));
                Ok(())
            },
            || panic!("failed routing must not retire the local copy"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn full_local_queue_still_allows_home_acceptance_but_never_false_success() {
        for routed in [None, Some(DispatchAckP::Forwarded { accepted_at_ms: 42 })] {
            let expect_success = routed.is_some();
            super::accept_dispatch(
                async { Ok(DispatchAckP::QueueFull) },
                async { routed },
                |ack| async move {
                    assert_eq!(matches!(ack, DispatchAckP::Forwarded { .. }), expect_success);
                    if !expect_success {
                        assert!(matches!(ack, DispatchAckP::QueueFull));
                    }
                    Ok(())
                },
                || panic!("no accepted local copy to retire"),
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn storage_failure_and_id_collision_cannot_be_acknowledged_as_accepted() {
        let result = super::accept_dispatch(
            async { anyhow::bail!("disk write failed") },
            async { panic!("do not route a failed write") },
            |_| async { panic!("do not claim acceptance of a failed write") },
            || panic!("nothing to retire"),
        )
        .await;
        assert!(result.is_err());
        super::accept_dispatch(
            async { Ok(DispatchAckP::Error { reason: "id collision".into() }) },
            async { panic!("do not route around an id collision") },
            |ack| async move {
                assert!(matches!(ack, DispatchAckP::Error { .. }));
                Ok(())
            },
            || panic!("nothing to retire"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn queued_retry_is_deduplicated_and_cleanup_preserves_other_dispatches() {
        use common::proto::client_rel::DispatchP;

        use super::dispatch_to_deliver;
        use super::retire_local_copy;
        use super::store_in_rocks;
        use crate::storage::db::Store;
        let path = std::env::temp_dir().join(format!("pz-forward-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let store = Store::open(&path).unwrap();
        let mut fwd = DispatchP {
            to:             [1u8; 32].into(),
            from:           [2u8; 32].into(),
            id:             [3u8; 16].into(),
            payload:        vec![4u8].into(),
            sig:            [5u8; 64].into(),
            accepted_at_ms: 42,
            wake:           common::proto::client_rel::Wake::No,
            ttl_ms:         0,
        };
        let queued = store_in_rocks(&store, fwd.to, &dispatch_to_deliver(&fwd)).await.unwrap();
        assert!(matches!(queued, DispatchAckP::Queued { .. }));
        fwd.accepted_at_ms = 43;
        store_in_rocks(&store, fwd.to, &dispatch_to_deliver(&fwd)).await.unwrap();
        assert_eq!(store.messages.len().unwrap(), 1, "retry cannot add a second copy");
        let mut other = fwd.clone();
        other.id = [6u8; 16].into();
        store_in_rocks(&store, other.to, &dispatch_to_deliver(&other)).await.unwrap();
        // A colliding ID from a different author must not retire the original.
        let mut collision = fwd.clone();
        collision.from = [7u8; 32].into();
        retire_local_copy(&store, &collision).unwrap();
        assert_eq!(store.messages.len().unwrap(), 2);
        retire_local_copy(&store, &fwd).unwrap();
        assert_eq!(store.messages.len().unwrap(), 1, "cleanup finds the earlier ingress key");
        retire_local_copy(&store, &other).unwrap();
        assert_eq!(store.messages.len().unwrap(), 0);
        drop(store);
        std::fs::remove_dir_all(path).unwrap();
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
