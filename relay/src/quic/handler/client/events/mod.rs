use anyhow::Result;
use client_handler::AckAuthPayload;
use client_handler::ClientCtxHandle;
use common::proto::client_rel::CRelayPacket;
use forward::handle_forward;
use misc::handle_misc;
use quinn::SendStream;

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
        DrainAuth { timestamp, sig } => {
            handle_drain_auth(ctx.clone(), timestamp, sig.0).await
        },
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
        // wrapper sig + skew, originates the peer/1 fan-out, and
        // replies with the matching SRelayPacket (or DhtUnavailable
        // when this relay has DHT disabled).
        PublishKeyPackage { records, timestamp, mode, sig } => {
            mls_relay::handle_publish_keypackage(
                ctx.clone(), records, timestamp, mode, sig.0, tx,
            )
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

        Ephemeral(eph) => forward::handle_ephemeral(eph, ctx.clone()).await,

        // Ignore Extra
        _ => Ok(()),
    }
}
