use anyhow::Result;
use common::msg::pack::Unpacker;
use common::msg::relay::RelayPacket;

use crate::quic::handler::client::ClientCtxHandle;
use crate::quic::handler::client::events::handshake::handle_handshake;
use crate::quic::handler::client::events::misc::handle_misc;

pub mod handshake;
pub mod misc;

pub(super) async fn handle_packet(bytes: &[u8], ctx: ClientCtxHandle) -> Result<()> {
    println!("PACKET : {:?}", bytes);
    use RelayPacket::*;
    match RelayPacket::from_cbor(bytes)? {
        Handshake(packet) => handle_handshake(packet, ctx.clone()).await,
        Misc(packet) => handle_misc(packet, ctx.clone()).await,
    }
}
