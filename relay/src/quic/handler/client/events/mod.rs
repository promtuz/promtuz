use anyhow::Result;
use common::proto::client_rel::RelayPacket;
use quinn::SendStream;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::forward::handle_forward;
use crate::quic::handler::client::events::handshake::handle_handshake;
use crate::quic::handler::client::events::misc::handle_misc;

pub mod forward;
pub mod handshake;
pub mod misc;

pub(super) async fn handle_packet(
    packet: RelayPacket, ctx: ClientCtxHandle, tx: &mut SendStream,
) -> Result<()> {
    use RelayPacket::*;

    match packet {
        Handshake(packet) => handle_handshake(packet, ctx.clone(), tx).await,
        Misc(packet) => handle_misc(packet, ctx.clone(), tx).await,
        Forward(fwd) => handle_forward(fwd, ctx.clone(), tx).await,
        // ForwardResult and Deliver are relay→client only, never sent by clients
        ForwardResult(_) | Deliver(_) => Ok(()),
    }
}
