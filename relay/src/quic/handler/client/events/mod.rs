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

/// Bounds opening an outbound stream: a remote that grants no stream credit would pin the task.
pub(crate) const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Detaches `task` and drops it when `cancel` fires. Callers pass the process token, so the task
/// can outlive the connection that started it.
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
        Query(query) => handle_misc(query, ctx.clone(), tx).await,
        Dispatch(fwd) => handle_forward(fwd, ctx.clone(), tx).await,
        DrainQueue => handle_drain_queue(ctx.clone(), tx).await,
        AckDrain { ids } => handle_ack_drain(ctx.clone(), ids, tx).await,
        DrainAuth { timestamp, sig } => handle_drain_auth(ctx.clone(), timestamp, sig.0).await,
        // Answers the round `run_remote_ack_round` parked; an unsolicited `AckAuth` is dropped.
        AckAuth { sig, timestamp } => {
            if let Some(sender) = ctx.ack_auth.lock().take() {
                let _ = sender.send(AckAuthPayload { sig: sig.0, timestamp });
            }
            Ok(())
        },

        PublishKeyPackage { records, timestamp, sig } => {
            mls_relay::handle_publish_keypackage(ctx.clone(), records, timestamp, sig.0, tx).await
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

        TurnCredentials => misc::handle_turn_credentials(ctx.clone(), tx).await,
        KeyPackageInventory { request } => {
            mls_relay::handle_keypackage_inventory(ctx.clone(), &request.0, tx).await
        },
        ServiceCapabilities => {
            use common::proto::{Sender, client_rel::SRelayPacket};
            let support=if ctx.relay.dht.is_some() { crate::dht::mls::service_support() } else { common::contracts::Support::default() };
            SRelayPacket::ServiceCapabilities { supported:support.encode().into() }.send(tx).await?;
            Ok(())
        },

        _ => Ok(()),
    }
}
