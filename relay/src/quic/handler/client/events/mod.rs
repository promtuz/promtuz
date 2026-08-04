use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use client_handler::AckAuthPayload;
use client_handler::ClientCtxHandle;
use common::proto::client_rel::CRelayPacket;
use forward::handle_forward;
use misc::handle_misc;
use quinn::SendStream;
use tokio_util::sync::CancellationToken;

use crate::quic::handler::client::events::drain::handle_ack_drain;
use crate::quic::handler::client::events::drain::handle_drain_queue;
use crate::quic::handler::client::events::drain_auth::handle_drain_auth;
use crate::quic::handler::client::{
    self as client_handler,
};

pub mod drain;
pub mod drain_auth;
pub mod forward;
pub mod misc;
pub mod mls_relay;
pub mod presence;

/// Budget for opening an outbound stream to a client or peer. A remote that
/// grants no stream credit otherwise pins the calling task forever.
pub(crate) const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Detach `task`, dropping it when `cancel` fires so it cannot outlive the
/// connection handler that started it.
pub(crate) fn spawn_tied<F>(cancel: &CancellationToken, task: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = task => {},
        }
    });
}

/// Drive `tasks` to completion with at most `concurrency` in flight.
pub(crate) async fn bounded_fanout<F>(tasks: Vec<F>, concurrency: usize)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut queued = tasks.into_iter();
    let mut set = tokio::task::JoinSet::new();
    for task in queued.by_ref().take(concurrency.max(1)) {
        set.spawn(task);
    }
    while set.join_next().await.is_some() {
        if let Some(task) = queued.next() {
            set.spawn(task);
        }
    }
}

pub(super) async fn handle_packet(
    packet: CRelayPacket, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    use CRelayPacket::*;

    match packet {
        // Handshake(packet) => handle_handshake(packet, ctx.clone(), tx).await,
        Query(query) => handle_misc(query, ctx.clone(), tx).await,
        Dispatch(fwd) => handle_forward(fwd, ctx.clone(), tx).await,
        DrainQueue => handle_drain_queue(ctx.clone(), tx).await,
        AckDrain => handle_ack_drain(ctx.clone(), tx).await,
        // Sticky-home. The packet has no response; we drop
        // verification failures silently (a malicious client could
        // otherwise probe the verifier — see `drain_auth.rs`).
        DrainAuth { timestamp, sig } => handle_drain_auth(ctx.clone(), timestamp, sig.0).await,
        // Sticky-home. Hand-off to the parked `oneshot::Sender`
        // installed by `handle_ack_drain` before sending the
        // `AckAuthRequest`. If no sender is parked (out-of-order client
        // — sent AckAuth without our request), drop silently.
        AckAuth { sig, timestamp } => {
            if let Some(sender) = ctx.ack_auth.lock().take() {
                let _ = sender.send(AckAuthPayload { sig: sig.0, timestamp });
            }
            Ok(())
        },

        // Tier-1 MLS DHT-RPC wrappers. Each handler verifies the
        // wrapper sig + skew, originates the peer/5 fan-out, and
        // replies with the matching SRelayPacket (or DhtUnavailable
        // when this relay has DHT disabled).
        PublishKeyPackage { records, timestamp, mode, sig } => {
            mls_relay::handle_publish_keypackage(ctx.clone(), records, timestamp, mode, sig.0, tx)
                .await
        },
        FetchKeyPackage { target_ipk, timestamp, sig } => {
            mls_relay::handle_fetch_keypackage(ctx.clone(), target_ipk.0, timestamp, sig.0, tx)
                .await
        },
        PublishWelcome { envelope, timestamp, sig } => {
            mls_relay::handle_publish_welcome(ctx.clone(), envelope, timestamp, sig.0, tx).await
        },
        FetchWelcomes { timestamp, sig } => {
            mls_relay::handle_fetch_welcomes(ctx.clone(), timestamp, sig.0, tx).await
        },
        AckWelcomes { welcome_ids, timestamp, sig } => {
            mls_relay::handle_ack_welcomes(ctx.clone(), welcome_ids, timestamp, sig.0, tx).await
        },

        Activity(eph) => forward::handle_activity(eph, ctx.clone()).await,

        SubscribePresence(sub) => presence::handle_subscribe(sub, ctx.clone()).await,

        SetPresence(mode) => presence::handle_set_presence(mode, ctx.clone()).await,

        RegisterPush { pseudonym, timestamp, sig } => {
            misc::handle_register_push(pseudonym.0, timestamp, sig.0, ctx.clone()).await
        },

        // Ignore Extra
        _ => Ok(()),
    }
}
