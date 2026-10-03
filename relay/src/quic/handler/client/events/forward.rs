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
use common::proto::pack::Packer;
use common::proto::pack::Unpacker;
use common::trace;
use common::types::bytes::Bytes;
use common::utils::now_ms;
use quinn::Connection;
use quinn::ConnectionError;
use quinn::SendStream;

use crate::dht::forward::ForwardSummary;
use crate::dht::forward::activity_is_authentic;
use crate::dht::forward::forward_to_homes;
use crate::dht::forward::verify_dispatch_user_sig;
use crate::dht::store::QueueAdmission;
use crate::dht::store::admit_to_queue;
use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::STREAM_OPEN_TIMEOUT;
use crate::quic::handler::client::events::spawn_tied;
use crate::quic::handler::client::remove_client_if_same;
use crate::storage::MessageKey;
use crate::storage::db::Store;

const ROUTE_TIMEOUT: Duration = Duration::from_secs(30);

const LIVE_DELIVER_ACK_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) async fn handle_forward(
    fwd: DispatchP, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    // Sender binding stays first: the signature check below would still pass for a forged
    // `from`, so nothing may run before `from` matches the authenticated session.
    if fwd.from.as_slice() != ctx.ipk.as_bytes().as_slice() {
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig).send(tx).await?;
        return Ok(());
    }
    // Over budget gets the full-queue answer: the client backs off and retries from its outbox.
    if ctx.limits.dispatch.check().is_err() {
        SRelayPacket::DispatchAck(DispatchAckP::QueueFull).send(tx).await?;
        return Ok(());
    }

    if !verify_dispatch_user_sig(&fwd) {
        SRelayPacket::DispatchAck(DispatchAckP::InvalidSig).send(tx).await?;
        return Ok(());
    }

    // Never accept a client-provided clock. This ingress relay owns the
    // display timestamp and carries it unchanged through every later hop.
    let accepted_at_ms = now_ms();
    let fwd = DispatchP { accepted_at_ms, ..fwd };

    let recipient = fwd.to;
    let delivery = dispatch_to_deliver(fwd.clone());
    accept_dispatch(
        store_in_rocks(&ctx.relay.store, recipient, &delivery),
        route_dispatch(fwd.clone(), &ctx),
        |ack| async move { SRelayPacket::DispatchAck(ack).send(tx).await.map_err(Into::into) },
        || retire_local_copy(&ctx.relay.store, &fwd),
    )
    .await
}

/// A durable local copy releases the sender. Routing continues after the reply, so a slow
/// recipient never gates acceptance and a lost sender ack never cancels delivery.
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

/// `None` leaves the accepted local copy for a later drain.
async fn route_dispatch(fwd: DispatchP, ctx: &ClientCtxHandle) -> Option<DispatchAckP> {
    let accepted_at_ms = fwd.accepted_at_ms;
    let recipient = fwd.to;
    let delivery = dispatch_to_deliver(fwd.clone());
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
        // Evict only a connection that is actually gone: evicting a live one on a bare timeout
        // would let any stranger knock a user offline with one junk dispatch.
        if conn.close_reason().is_some() {
            remove_client_if_same(&ctx.relay, &recipient.0, &conn);
        }
    }

    if let Some(dht) = ctx.relay.dht.as_ref().cloned() {
        match forward_to_homes(dht, fwd, accepted_at_ms).await {
            Ok(summary) => {
                return Some(ack_for_summary(&summary, accepted_at_ms));
            }
            Err(err) => {
                trace!(
                    "FORWARD: K-closest fan-out fell back to local queue: {err}"
                );
            }
        }
    }

    None
}

/// Removes only copies of this exact signed dispatch: a retry can carry a different ingress
/// timestamp, and another sender's colliding id must never be deleted.
fn retire_local_copy(store: &Store, fwd: &DispatchP) -> Result<()> {
    let mut batch = store.batch();
    for entry in store.messages.prefix(fwd.to.0) {
        let (key, value) = entry.into_inner()?;
        let Some(key_fields) = MessageKey::parse(&key) else { continue };
        if key_fields.id == fwd.id.0
            && let Ok(queued) = DeliverP::deser(&value)
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

/// Ephemeral signals are never queued. The signature is checked here because a K-way fan-out is
/// too costly to spend on unauthenticated bytes.
pub(super) async fn handle_activity(eph: ActivityP, ctx: ClientCtxHandle) -> Result<()> {
    if ctx.limits.dispatch.check().is_err() {
        return Ok(());
    }
    if eph.from.as_slice() != ctx.ipk.as_bytes().as_slice() {
        return Ok(());
    }
    if !activity_is_authentic(&eph, now_ms()) {
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

fn ack_for_summary(summary: &ForwardSummary, accepted_at_ms: u64) -> DispatchAckP {
    if summary.any_delivered() {
        DispatchAckP::Delivered { accepted_at_ms }
    } else {
        DispatchAckP::Forwarded { accepted_at_ms }
    }
}

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

pub(crate) fn dispatch_to_deliver(d: DispatchP) -> DeliverP {
    DeliverP {
        id:      d.id,
        from:    d.from,
        payload: d.payload,
        sig:     d.sig,
        accepted_at_ms: d.accepted_at_ms,
        ttl_ms:  d.ttl_ms,
    }
}

async fn store_in_rocks(
    store: &Store, recipient: Bytes<32>, delivery: &DeliverP,
) -> Result<DispatchAckP> {
    debug!(
        "dispatch {}: recipient {} — accepting in local queue",
        hex::encode(&delivery.id.0[..8]),
        hex::encode(&recipient.0[..8])
    );

    let admission = {
        let _admission = store.admission(&recipient.0);
        let admission =
            admit_to_queue(&store.messages, &recipient.0, &delivery.id.0, &delivery.from.0, |v| {
                DeliverP::deser(v).ok().map(|d| d.from.0)
            });
        if matches!(admission, QueueAdmission::Insert) {
            let key = MessageKey::new(&recipient.0, delivery.accepted_at_ms, &delivery.id.0);
            store.put_sync(&store.messages, key.as_bytes(), delivery.ser()?)?;
        }
        admission
    };
    match admission {
        // `Queued` is a durability promise, so the reply waits for the barrier, which resolves
        // once the group commit covering the write is on disk.
        QueueAdmission::Insert | QueueAdmission::AlreadyQueued => {
            store.persist_barrier().wait().await?;
            Ok(DispatchAckP::Queued { accepted_at_ms: delivery.accepted_at_ms })
        },
        QueueAdmission::IdTakenByOther => {
            Ok(DispatchAckP::Error { reason: "dispatch id already queued".into() })
        },
        QueueAdmission::ScanFailed => Ok(DispatchAckP::Error { reason: "queue scan failed".into() }),
        QueueAdmission::Full => {
            trace!("FORWARD: queue full for recipient {}; rejecting", hex::encode(recipient));
            Ok(DispatchAckP::QueueFull)
        },
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::test_support::dispatch;
    use crate::test_support::key;

    const QUEUED: DispatchAckP = DispatchAckP::Queued { accepted_at_ms: 42 };

    #[tokio::test(start_paused = true)]
    async fn a_slow_recipient_neither_delays_acceptance_nor_loses_the_fallback() {
        let start = tokio::time::Instant::now();
        let (durable, replied) = (Cell::new(false), Cell::new(false));
        accept_dispatch(
            async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                durable.set(true);
                Ok(QUEUED)
            },
            async {
                assert!(replied.get(), "the recipient is contacted only after the sender's ack");
                std::future::pending().await
            },
            |ack| {
                assert_eq!(ack, QUEUED);
                async {
                    assert!(durable.get(), "never acknowledged before the persistence barrier");
                    assert_eq!(start.elapsed(), Duration::from_millis(10));
                    replied.set(true);
                    Ok(())
                }
            },
            || panic!("a timed-out route must keep the durable fallback"),
        )
        .await
        .unwrap();
        assert_eq!(start.elapsed(), Duration::from_millis(10) + ROUTE_TIMEOUT);
    }

    #[tokio::test]
    async fn successful_routing_retires_the_fallback_even_if_the_sender_left() {
        for routed in [
            DispatchAckP::Delivered { accepted_at_ms: 42 },
            DispatchAckP::Forwarded { accepted_at_ms: 42 },
        ] {
            let retired = Cell::new(false);
            let result = accept_dispatch(
                async { Ok(QUEUED) },
                async { Some(routed) },
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
    async fn a_failed_route_keeps_the_accepted_copy() {
        accept_dispatch(
            async { Ok(QUEUED) },
            async { None },
            |ack| async move {
                assert_eq!(ack, QUEUED);
                Ok(())
            },
            || panic!("a failed route must not retire the local copy"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_full_local_queue_can_still_be_routed_but_never_reports_false_success() {
        let forwarded = || DispatchAckP::Forwarded { accepted_at_ms: 42 };
        for (routed, expected) in
            [(None, DispatchAckP::QueueFull), (Some(forwarded()), forwarded())]
        {
            accept_dispatch(
                async { Ok(DispatchAckP::QueueFull) },
                async { routed },
                |ack| async move {
                    assert_eq!(ack, expected);
                    Ok(())
                },
                || panic!("there is no accepted local copy to retire"),
            )
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn a_failed_write_or_id_collision_is_never_acknowledged_as_accepted() {
        let result = accept_dispatch(
            async { anyhow::bail!("disk write failed") },
            async { panic!("do not route a failed write") },
            |_| async { panic!("do not acknowledge a failed write") },
            || panic!("nothing to retire"),
        )
        .await;
        assert!(result.is_err());
        accept_dispatch(
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

    /// A retry is stored once, and retiring deletes only this author's copy, whatever ingress
    /// time the retry carried.
    #[tokio::test]
    async fn a_retried_dispatch_is_stored_once_and_retired_only_for_its_author() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_empty(dir.path());
        let queue = async |sent: &DispatchP| {
            store_in_rocks(&store, sent.to, &dispatch_to_deliver(sent.clone())).await.unwrap()
        };
        let mut sent = dispatch(&key(1), [2; 32], [3; 16], b"once");
        assert!(matches!(queue(&sent).await, DispatchAckP::Queued { .. }));
        sent.accepted_at_ms = 43;
        assert!(matches!(queue(&sent).await, DispatchAckP::Queued { .. }));
        assert_eq!(store.messages.len().unwrap(), 1, "a retry adds no second copy");
        let other = dispatch(&key(1), [2; 32], [6; 16], b"other");
        queue(&other).await;

        let collision = dispatch(&key(7), [2; 32], [3; 16], b"once");
        retire_local_copy(&store, &collision).unwrap();
        assert_eq!(
            store.messages.len().unwrap(),
            2,
            "another author's colliding id deletes nothing"
        );
        retire_local_copy(&store, &sent).unwrap();
        assert_eq!(store.messages.len().unwrap(), 1, "the earlier ingress key is found");
        retire_local_copy(&store, &other).unwrap();
        assert_eq!(store.messages.len().unwrap(), 0);
    }
}
