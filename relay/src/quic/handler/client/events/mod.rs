use anyhow::Result;
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
        // Sticky-home phase 2c. The packet has no response; we drop
        // verification failures silently (a malicious client could
        // otherwise probe the verifier — see `drain_auth.rs`).
        DrainAuth { timestamp, sig } => {
            handle_drain_auth(ctx.clone(), timestamp, sig.0).await
        },

        // Ignore Extra
        _ => Ok(()),
    }
}
