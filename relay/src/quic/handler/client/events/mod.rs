use anyhow::Result;
use client_handler::ClientCtxHandle;
use common::proto::client_rel::CRelayPacket;
use forward::handle_forward;
use misc::handle_misc;
use quinn::SendStream;

use crate::quic::handler::client::events::drain::handle_ack_drain;
use crate::quic::handler::client::events::drain::handle_drain_queue;
use crate::quic::handler::client::{
    self as client_handler,
};

pub mod drain;
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

        // Ignore Extra
        _ => Ok(()),
    }
}
