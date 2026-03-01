use anyhow::Result;
use common::proto::client_rel::CRelayPacket;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::forward::handle_forward;
use crate::quic::handler::client::events::misc::handle_misc;

pub mod forward;
pub mod handshake;
pub mod misc;

pub(super) async fn handle_packet(
    packet: CRelayPacket, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    use CRelayPacket::*;

    match packet {
        // Handshake(packet) => handle_handshake(packet, ctx.clone(), tx).await,
        Query(query) => handle_misc(query, ctx.clone(), tx).await,
        Forward(fwd) => handle_forward(fwd, ctx.clone(), tx).await,
    }
}
